//! UI-independent core of kpiddle, PIDdle for a Kubernetes cluster.
//!
//! * [`model`] — pods, metrics samples and the [`model::Cluster`] that turns
//!   them into a [`model::Snapshot`] grouped by namespace and workload.
//! * [`quantity`] — Kubernetes resource quantities (`250m`, `1Gi`) as numbers.
//!
//! Missing or stale metrics never look like zero usage: every pod's usage is
//! fresh, stale (with its age) or missing, and totals over pods say how many
//! of their members were measured. See `docs/kpiddle/DESIGN.md`.

pub mod model;
pub mod quantity;
