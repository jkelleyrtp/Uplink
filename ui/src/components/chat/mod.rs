use kit::components::nav::Route;

pub mod compose;
pub mod create_group;
pub mod edit_group;
pub mod group_users;
pub mod sidebar;
pub mod welcome;

#[derive(PartialEq, Clone)]
pub struct RouteInfo {
    pub routes: Vec<Route>,
    pub active: Route,
}
