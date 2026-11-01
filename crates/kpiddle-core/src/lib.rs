//! UI-independent core of kpiddle, PIDdle for a Kubernetes cluster.
//!
//! * [`actions`] — evict, delete and rollout restart, and how risky each is.
//! * [`model`] — pods, metrics samples and the [`model::Cluster`] that turns
//!   them into a [`model::Snapshot`] grouped by namespace and workload.
//! * [`quantity`] — Kubernetes resource quantities (`250m`, `1Gi`) as numbers.
//! * [`source`] — feeds a `Cluster` from a live API server via kube-rs.
//!
//! Missing or stale metrics never look like zero usage: every pod's usage is
//! fresh, stale (with its age) or missing, and totals over pods say how many
//! of their members were measured. See `docs/DESIGN.md`.

pub mod actions;
pub mod model;
pub mod quantity;
pub mod source;
