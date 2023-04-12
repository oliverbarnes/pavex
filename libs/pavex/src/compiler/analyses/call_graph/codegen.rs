use ahash::{HashMap, HashMapExt};
use bimap::BiHashMap;
use fixedbitset::FixedBitSet;
use guppy::PackageId;
use indexmap::IndexMap;
use petgraph::graph::NodeIndex;
use petgraph::prelude::{DfsPostOrder, EdgeRef};
use petgraph::visit::Reversed;
use petgraph::Direction;
use proc_macro2::{Ident, TokenStream};
use quote::{quote, ToTokens};
use syn::ItemFn;

use crate::compiler::analyses::call_graph::core_graph::{CallGraphEdgeMetadata, RawCallGraph};
use crate::compiler::analyses::call_graph::{CallGraph, CallGraphNode, NumberOfAllowedInvocations};
use crate::compiler::analyses::components::{ComponentDb, HydratedComponent};
use crate::compiler::analyses::computations::ComputationDb;
use crate::compiler::codegen_utils;
use crate::compiler::codegen_utils::{Fragment, VariableNameGenerator};
use crate::compiler::computation::{Computation, MatchResultVariant};
use crate::compiler::constructors::Constructor;
use crate::language::ResolvedType;

/// Generate the dependency closure of the [`CallGraph`]'s root callable.
///
/// See [`CallGraph`] docs for more details.
pub(crate) fn codegen_callable_closure(
    call_graph: &CallGraph,
    package_id2name: &BiHashMap<PackageId, String>,
    component_db: &ComponentDb,
    computation_db: &ComputationDb,
) -> Result<ItemFn, anyhow::Error> {
    let input_parameter_types = call_graph.required_input_types();
    let mut variable_generator = VariableNameGenerator::new();
    // Assign a unique parameter name to each input parameter type.
    let parameter_bindings: HashMap<ResolvedType, Ident> = input_parameter_types
        .iter()
        .map(|type_| {
            let parameter_name = variable_generator.generate();
            (type_.to_owned(), parameter_name)
        })
        .collect();
    let CallGraph {
        call_graph,
        root_node_index: root_callable_node_index,
    } = call_graph;
    let body = codegen_callable_closure_body(
        *root_callable_node_index,
        call_graph,
        &parameter_bindings,
        package_id2name,
        component_db,
        computation_db,
        &mut variable_generator,
    )?;

    let function = {
        let inputs = input_parameter_types.iter().map(|type_| {
            let variable_name = &parameter_bindings[type_];
            let variable_type = type_.syn_type(package_id2name);
            quote! { #variable_name: #variable_type }
        });
        let component_id = match &call_graph[*root_callable_node_index] {
            CallGraphNode::Compute { component_id, .. } => component_id,
            n => {
                dbg!(n);
                unreachable!()
            }
        };
        let output_type = component_db
            .hydrated_component(*component_id, computation_db)
            .output_type()
            .syn_type(package_id2name);
        syn::parse2(quote! {
            pub async fn handler(#(#inputs),*) -> #output_type {
                #body
            }
        })
        .unwrap()
    };
    Ok(function)
}

/// Generate the function body for the dependency closure of the [`CallGraph`]'s root callable.
///
/// See [`CallGraph`] docs for more details.
fn codegen_callable_closure_body(
    root_callable_node_index: NodeIndex,
    call_graph: &RawCallGraph,
    parameter_bindings: &HashMap<ResolvedType, Ident>,
    package_id2name: &BiHashMap<PackageId, String>,
    component_db: &ComponentDb,
    computation_db: &ComputationDb,
    variable_name_generator: &mut VariableNameGenerator,
) -> Result<TokenStream, anyhow::Error> {
    let mut at_most_once_constructor_blocks = IndexMap::<NodeIndex, TokenStream>::new();
    let mut blocks = HashMap::<NodeIndex, Fragment>::new();
    let mut dfs = DfsPostOrder::new(Reversed(call_graph), root_callable_node_index);
    _codegen_callable_closure_body(
        root_callable_node_index,
        call_graph,
        parameter_bindings,
        package_id2name,
        component_db,
        computation_db,
        variable_name_generator,
        &mut at_most_once_constructor_blocks,
        &mut blocks,
        &mut dfs,
    )
}

fn _codegen_callable_closure_body(
    node_index: NodeIndex,
    call_graph: &RawCallGraph,
    parameter_bindings: &HashMap<ResolvedType, Ident>,
    package_id2name: &BiHashMap<PackageId, String>,
    component_db: &ComponentDb,
    computation_db: &ComputationDb,
    variable_name_generator: &mut VariableNameGenerator,
    at_most_once_constructor_blocks: &mut IndexMap<NodeIndex, TokenStream>,
    blocks: &mut HashMap<NodeIndex, Fragment>,
    dfs: &mut DfsPostOrder<NodeIndex, FixedBitSet>,
) -> Result<TokenStream, anyhow::Error> {
    let terminal_index = find_terminal_descendant(node_index, call_graph);
    // We want to start the code-generation process from a `MatchBranching` node with
    // no `MatchBranching` predecessors.
    // This ensures that we don't have to look-ahead when generating code for its predecessors.
    let traversal_start_index =
        find_match_branching_ancestor(terminal_index, call_graph, &dfs.finished)
            // If there are no `MatchBranching` nodes in the ancestors sub-graph, we start from the
            // the terminal node.
            .unwrap_or(terminal_index);
    dfs.move_to(traversal_start_index);
    while let Some(current_index) = dfs.next(Reversed(call_graph)) {
        let current_node = &call_graph[current_index];
        match current_node {
            CallGraphNode::Compute {
                component_id,
                n_allowed_invocations,
            } => {
                let computation = component_db
                    .hydrated_component(*component_id, computation_db)
                    .computation();
                match computation {
                    Computation::Callable(callable) => {
                        let block = codegen_utils::codegen_call_block(
                            get_node_type_inputs(
                                current_index,
                                call_graph,
                                component_db,
                                computation_db,
                            ),
                            callable.as_ref(),
                            blocks,
                            variable_name_generator,
                            package_id2name,
                        )?;
                        // This is the last node!
                        // We don't need to assign its value to a variable.
                        if current_index == traversal_start_index
                            // Or this is a single-use value, so no point in binding it to a variable.
                            || n_allowed_invocations == &NumberOfAllowedInvocations::Multiple
                        {
                            blocks.insert(current_index, block);
                        } else {
                            // We bind the constructed value to a variable name and instruct
                            // all dependents to refer to the constructed value via that
                            // variable name.
                            let parameter_name = variable_name_generator.generate();
                            let block = quote! {
                                let #parameter_name = #block;
                            };
                            at_most_once_constructor_blocks.insert(current_index, block);
                            blocks
                                .insert(current_index, Fragment::VariableReference(parameter_name));
                        }
                    }
                    Computation::MatchResult(_) => {
                        // We already bound the match result to a variable name when handling
                        // its parent `MatchBranching` node.
                    }
                }
            }
            CallGraphNode::InputParameter(input_type) => {
                let parameter_name = parameter_bindings[input_type].clone();
                blocks.insert(current_index, Fragment::VariableReference(parameter_name));
            }
            CallGraphNode::MatchBranching => {
                let variants = call_graph
                    .neighbors_directed(current_index, Direction::Outgoing)
                    .collect::<Vec<_>>();
                assert_eq!(2, variants.len());
                assert_eq!(current_index, traversal_start_index);
                let mut ok_arm = None;
                let mut err_arm = None;
                for variant_index in variants {
                    let mut at_most_once_constructor_blocks = IndexMap::new();
                    let mut variant_name_generator = variable_name_generator.clone();
                    let match_binding_parameter_name = variant_name_generator.generate();
                    let mut variant_blocks = {
                        let mut b = blocks.clone();
                        b.insert(
                            variant_index,
                            Fragment::VariableReference(match_binding_parameter_name.clone()),
                        );
                        b
                    };
                    // This `.clone()` is **load-bearing**.
                    // The sub-graph for each match arm might have one or more nodes in common.
                    // If we don't create a new DFS for each match arm, the visitor will only
                    // pick up the shared nodes once (for the first match arm), leading to issues
                    // when generating code for the second match arm (i.e. most likely a panic).
                    let mut new_dfs = dfs.clone();
                    let match_arm_body = _codegen_callable_closure_body(
                        variant_index,
                        call_graph,
                        parameter_bindings,
                        package_id2name,
                        component_db,
                        computation_db,
                        &mut variant_name_generator,
                        &mut at_most_once_constructor_blocks,
                        &mut variant_blocks,
                        &mut new_dfs,
                    )?;
                    let variant_type = match &call_graph[variant_index] {
                        CallGraphNode::Compute { component_id, .. } => {
                            match component_db.hydrated_component(*component_id, computation_db) {
                                HydratedComponent::Transformer(Computation::MatchResult(m))
                                | HydratedComponent::Constructor(Constructor(
                                    Computation::MatchResult(m),
                                )) => m.variant,
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    };
                    let match_arm_binding = match variant_type {
                        MatchResultVariant::Ok => {
                            quote! {
                                Ok(#match_binding_parameter_name)
                            }
                        }
                        MatchResultVariant::Err => {
                            quote! {
                                Err(#match_binding_parameter_name)
                            }
                        }
                    };
                    let match_arm = quote! {
                        #match_arm_binding => {
                            #match_arm_body
                        },
                    };
                    match variant_type {
                        MatchResultVariant::Ok => {
                            ok_arm = Some(match_arm);
                        }
                        MatchResultVariant::Err => err_arm = Some(match_arm),
                    }
                }
                // We do this to make sure that the Ok arm is always before the Err arm in the
                // generated code.
                let ok_arm = ok_arm.unwrap();
                let err_arm = err_arm.unwrap();
                let result_node_index = call_graph
                    .neighbors_directed(current_index, Direction::Incoming)
                    .next()
                    .unwrap();
                let result_binding = &blocks[&result_node_index];
                let block = quote! {
                    {
                        match #result_binding {
                            #ok_arm
                            #err_arm
                        }
                    }
                };
                blocks.insert(current_index, Fragment::Block(syn::parse2(block).unwrap()));
            }
        }
    }
    let body = {
        let at_most_once_constructors = at_most_once_constructor_blocks.values();
        // Remove the wrapping block, if there is one
        let b = match &blocks[&traversal_start_index] {
            Fragment::Block(b) => {
                let s = &b.stmts;
                quote! { #(#s)* }
            }
            Fragment::Statement(b) => b.to_token_stream(),
            Fragment::VariableReference(n) => n.to_token_stream(),
        };
        quote! {
            #(#at_most_once_constructors)*
            #b
        }
    };
    Ok(body)
}

/// Returns a terminal descendant of the given node—i.e. a node that is reachable from
/// `start_index` and has no outgoing edges.
fn find_terminal_descendant(start_index: NodeIndex, call_graph: &RawCallGraph) -> NodeIndex {
    let mut dfs = DfsPostOrder::new(call_graph, start_index);
    while let Some(node_index) = dfs.next(call_graph) {
        let mut successors = call_graph.neighbors_directed(node_index, Direction::Outgoing);
        if successors.next().is_none() {
            return node_index;
        }
    }
    // `call_graph` is a DAG, so we should never reach this point.
    unreachable!()
}

/// Returns `Some(node_index)` if there is an ancestor (either directly or indirectly connected
/// to `start_index`) that is a `CallGraphNode::MatchBranching` and doesn't belong to `ignore_set`.
/// `node` index won't have any ancestors that are themselves a `CallGraphNode::MatchBranching`.
///
/// Returns `None` if such an ancestor doesn't exist.
fn find_match_branching_ancestor(
    start_index: NodeIndex,
    call_graph: &RawCallGraph,
    ignore_set: &FixedBitSet,
) -> Option<NodeIndex> {
    let mut ancestors = DfsPostOrder::new(Reversed(call_graph), start_index);
    while let Some(ancestor_index) = ancestors.next(Reversed(call_graph)) {
        if ancestor_index == start_index {
            continue;
        }
        if ignore_set.contains(ancestor_index.index()) {
            continue;
        }
        if let CallGraphNode::MatchBranching { .. } = &call_graph[ancestor_index] {
            return Some(ancestor_index);
        }
    }
    None
}

fn get_node_type_inputs<'a, 'b: 'a>(
    node_index: NodeIndex,
    call_graph: &'a RawCallGraph,
    component_db: &'b ComponentDb,
    computation_db: &'b ComputationDb,
) -> impl Iterator<Item = (NodeIndex, ResolvedType, CallGraphEdgeMetadata)> + 'a {
    call_graph
        .edges_directed(node_index, Direction::Incoming)
        .map(move |edge| {
            let node = &call_graph[edge.source()];
            let type_ = match node {
                CallGraphNode::Compute { component_id, .. } => {
                    let component = component_db.hydrated_component(*component_id, computation_db);
                    component.output_type().to_owned()
                }
                CallGraphNode::InputParameter(i) => i.to_owned(),
                CallGraphNode::MatchBranching => unreachable!(),
            };
            (edge.source(), type_, edge.weight().to_owned())
        })
}