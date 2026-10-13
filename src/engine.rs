//! The Docker Engine operations the synchronizer depends on, behind a seam that
//! tests can replace with a scripted fake.

use bollard::errors::Error;
use bollard::models::{ContainerInspectResponse, ContainerSummary, EventMessage};
use bollard::query_parameters::{EventsOptions, InspectContainerOptions, ListContainersOptions};
use bollard::Docker;
use std::fmt;
use std::pin::Pin;
use tokio_stream::Stream;

/// A stream of Docker events, as returned by [`Engine::events`].
pub type Events<'a> = Pin<Box<dyn Stream<Item = Result<EventMessage, Error>> + Send + 'a>>;

/// Dispatches Docker Engine calls to a real daemon, or to a fake in tests.
pub enum Engine {
    Native(Docker),
    #[cfg(test)]
    Fake(fake::Ctrl),
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(_) => f.write_str("Engine::Native"),
            #[cfg(test)]
            Self::Fake(ctrl) => f.debug_tuple("Engine::Fake").field(ctrl).finish(),
        }
    }
}

impl Engine {
    /// Lists the containers that are currently running.
    pub async fn list_running(&self) -> Result<Vec<ContainerSummary>, Error> {
        match self {
            // Without options, Docker lists only running containers.
            Self::Native(docker) => docker.list_containers(None::<ListContainersOptions>).await,
            #[cfg(test)]
            Self::Fake(ctrl) => Ok(ctrl.list_running()),
        }
    }

    /// Returns the full inspection data for container `id`.
    pub async fn inspect(&self, id: &str) -> Result<ContainerInspectResponse, Error> {
        match self {
            Self::Native(docker) => {
                docker
                    .inspect_container(id, None::<InspectContainerOptions>)
                    .await
            }
            #[cfg(test)]
            Self::Fake(ctrl) => ctrl.inspect(id),
        }
    }

    /// Subscribes to the daemon's event stream.
    pub fn events(&self, options: EventsOptions) -> Events<'_> {
        match self {
            Self::Native(docker) => Box::pin(docker.events(Some(options))),
            #[cfg(test)]
            Self::Fake(ctrl) => ctrl.events(options),
        }
    }
}

/// A scripted stand-in for the Docker daemon.
#[cfg(test)]
pub mod fake {
    use super::{Error, Events};
    use bollard::models::{ContainerInspectResponse, ContainerSummary, EventMessage};
    use bollard::query_parameters::EventsOptions;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, PoisonError};
    use tokio_stream::StreamExt;

    /// Controls what the fake daemon reports, and records how it was called.
    ///
    /// Cloning shares the same state, so a test keeps one handle while the
    /// synchronizer owns another.
    #[derive(Debug, Clone, Default)]
    pub struct Ctrl {
        inner: Arc<Mutex<Inner>>,
    }

    #[derive(Debug, Default)]
    struct Inner {
        containers: BTreeMap<String, ContainerInspectResponse>,
        events: Vec<EventMessage>,
        subscriptions: Vec<EventsOptions>,
    }

    impl Ctrl {
        fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
            f(&mut self.inner.lock().unwrap_or_else(PoisonError::into_inner))
        }

        /// Makes `container` running, keyed by its `id`.
        pub fn start(&self, container: ContainerInspectResponse) {
            let id = container.id.clone().unwrap_or_default();
            self.with(|inner| inner.containers.insert(id, container));
        }

        /// Removes container `id`, as if it was stopped and removed.
        pub fn remove(&self, id: &str) {
            self.with(|inner| inner.containers.remove(id));
        }

        /// Queues `event` for the next subscriber.
        pub fn emit(&self, event: EventMessage) {
            self.with(|inner| inner.events.push(event));
        }

        /// The options of every event subscription made so far.
        pub fn subscriptions(&self) -> Vec<EventsOptions> {
            self.with(|inner| inner.subscriptions.clone())
        }

        pub(super) fn list_running(&self) -> Vec<ContainerSummary> {
            self.with(|inner| {
                inner
                    .containers
                    .keys()
                    .map(|id| ContainerSummary {
                        id: Some(id.clone()),
                        ..Default::default()
                    })
                    .collect()
            })
        }

        pub(super) fn inspect(&self, id: &str) -> Result<ContainerInspectResponse, Error> {
            self.with(|inner| inner.containers.get(id).cloned())
                .ok_or_else(|| Error::DockerResponseServerError {
                    status_code: 404,
                    message: format!("No such container: {id}"),
                })
        }

        /// Delivers the queued events, then stays open like a quiet daemon.
        pub(super) fn events(&self, options: EventsOptions) -> Events<'static> {
            let queued = self.with(|inner| {
                inner.subscriptions.push(options);
                std::mem::take(&mut inner.events)
            });
            Box::pin(tokio_stream::iter(queued.into_iter().map(Ok)).chain(tokio_stream::pending()))
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests should fail loudly")]
mod tests {
    use super::*;

    #[test]
    fn test_debug_names_the_backend_without_its_internals() {
        let native = Engine::Native(Docker::connect_with_socket_defaults().unwrap());

        assert_eq!(format!("{native:?}"), "Engine::Native");
    }
}
