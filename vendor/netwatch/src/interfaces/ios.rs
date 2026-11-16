//! iOS permits address enumeration, while routing sockets are unavailable.
pub(super) use super::netdev_impl::get_state;
pub(super) async fn default_route() -> Option<super::DefaultRouteDetails> { None }
pub(super) fn home_router() -> Option<super::HomeRouter> { None }
