//! Ownership of a new registration until startup returns its ID to the caller.

use crate::state::{AppState, ClientId, ServerId};

enum Registration {
    Server(ServerId),
    Client(ClientId),
}

pub(crate) struct StartupGuard {
    state: AppState,
    registration: Option<Registration>,
}

impl StartupGuard {
    pub(crate) fn server(state: &AppState, id: ServerId) -> Self {
        Self {
            state: state.clone(),
            registration: Some(Registration::Server(id)),
        }
    }

    pub(crate) fn client(state: &AppState, id: ClientId) -> Self {
        Self {
            state: state.clone(),
            registration: Some(Registration::Client(id)),
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.registration = None;
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };
        let state = self.state.clone();
        // Cancellation cannot await cleanup. Keep ownership in a cleanup future;
        // removal also aborts the instance's workers and its scheduled executions.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                match registration {
                    Registration::Server(id) => {
                        state.remove_server(id).await;
                    }
                    Registration::Client(id) => {
                        state.remove_client(id).await;
                    }
                }
            });
        }
    }
}
