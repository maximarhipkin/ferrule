//! M44 part 5: `/healthz` and `/busyz`. Answered before the page's own
//! host and session checks, so a container's health check and the panel
//! can ask without signing in.

use super::http::{Request, Response};
use super::Dashboard;

pub fn healthz(_dash: &Dashboard, _req: &Request) -> Response {
    Response::text(501, "not yet")
}

pub fn busyz(_dash: &Dashboard, _req: &Request) -> Response {
    Response::text(501, "not yet")
}
