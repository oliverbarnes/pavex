use pavex::blueprint::router::GET;
use pavex::blueprint::Blueprint;
use pavex::f;

pub fn blueprint() -> Blueprint {
    let mut bp = Blueprint::new();
    bp.route(GET, "/head", f!(crate::head::handler));
    bp
}
