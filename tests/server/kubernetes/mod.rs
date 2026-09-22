//! Kubernetes API server protocol E2E tests

#[cfg(all(test, feature = "kubernetes-server"))]
mod connection_bounds_test;

#[cfg(all(test, feature = "kubernetes-server"))]
mod e2e_test;

#[cfg(all(test, feature = "kubernetes-server"))]
mod guard_test;
