#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteKey {
    pub harness: String,
    pub session_id: String,
}

impl RouteKey {
    pub fn new(harness: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            harness: harness.into(),
            session_id: session_id.into(),
        }
    }

    pub fn from_cursor_key(cursor_key: &str) -> Option<Self> {
        let without_surface = cursor_key.strip_suffix(":hook")?;
        let (harness, session_id) = without_surface.split_once(':')?;
        if harness.is_empty() || session_id.is_empty() {
            return None;
        }
        Some(Self {
            harness: harness.to_owned(),
            session_id: session_id.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextQuery {
    pub route: Option<RouteKey>,
    pub through_event_id: Option<u64>,
}

impl ContextQuery {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn from_target(
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
        through_event_id: Option<u64>,
    ) -> Self {
        let route = match (target_harness, target_session_id) {
            (Some(harness), Some(session_id)) => Some(RouteKey::new(harness, session_id)),
            _ => None,
        };
        Self {
            route,
            through_event_id,
        }
    }
}
