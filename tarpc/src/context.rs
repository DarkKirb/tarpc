// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

//! Provides a request context that carries a deadline and trace context. This context is sent from
//! client to server and is used by the server to enforce response deadlines.

use crate::trace::{self, TraceId};
use opentelemetry::trace::TraceContextExt;
use static_assertions::assert_impl_all;
use std::{
    convert::TryFrom,
    time::{Duration, SystemTime},
};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// A request context that carries request-scoped information like deadlines and trace information.
/// It is sent from client to server and is used by the server to enforce response deadlines.
///
/// The context should not be stored directly in a server implementation, because the context will
/// be different for each request in scope.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
#[cfg_attr(feature = "serde1", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(feature = "rkyv", archive(check_bytes))]
pub struct Context {
    /// When the client expects the request to be complete by. The server should cancel the request
    /// if it is not complete by this time.
    #[cfg_attr(feature = "serde1", serde(default = "ten_seconds_from_now"))]
    // Serialized as a Duration to prevent clock skew issues.
    #[cfg_attr(feature = "serde1", serde(with = "absolute_to_relative_time"))]
    #[cfg_attr(feature = "rkyv", with(RkyvSystemTime))]
    pub deadline: SystemTime,
    /// Uniquely identifies requests originating from the same source.
    /// When a service handles a request by making requests itself, those requests should
    /// include the same `trace_id` as that included on the original request. This way,
    /// users can trace related actions across a distributed system.
    pub trace_context: trace::Context,
}

#[cfg(feature = "rkyv")]
struct RkyvSystemTime;

#[cfg(feature = "rkyv")]
impl rkyv::with::ArchiveWith<SystemTime> for RkyvSystemTime {
    type Archived = rkyv::Archived<Duration>;
    type Resolver = rkyv::Resolver<Duration>;

    unsafe fn resolve_with(field: &SystemTime, pos: usize, _: (), out: *mut Self::Archived) {
        use rkyv::Archive;
        let deadline = field
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        deadline.resolve(pos, (), out);
    }
}

#[cfg(feature = "rkyv")]
impl<S: rkyv::Fallible + ?Sized> rkyv::with::SerializeWith<SystemTime, S> for RkyvSystemTime
where
    Duration: rkyv::Serialize<S>,
{
    fn serialize_with(field: &SystemTime, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        use rkyv::Serialize;
        let deadline = field
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        deadline.serialize(serializer)
    }
}

#[cfg(feature = "rkyv")]
impl<D: rkyv::Fallible + ?Sized>
    rkyv::with::DeserializeWith<rkyv::Archived<Duration>, SystemTime, D> for RkyvSystemTime
where
    rkyv::Archived<Duration>: rkyv::Deserialize<Duration, D>,
{
    fn deserialize_with(
        field: &rkyv::Archived<Duration>,
        deserializer: &mut D,
    ) -> Result<SystemTime, D::Error> {
        use rkyv::Deserialize;
        Ok(SystemTime::UNIX_EPOCH + field.deserialize(deserializer)?)
    }
}

#[cfg(feature = "serde1")]
mod absolute_to_relative_time {
    pub use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub use std::time::{Duration, SystemTime};

    pub fn serialize<S>(deadline: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let deadline = deadline
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        deadline.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let deadline = Duration::deserialize(deserializer)?;
        Ok(SystemTime::now() + deadline)
    }

    #[cfg(test)]
    #[derive(serde::Serialize, serde::Deserialize)]
    struct AbsoluteToRelative(#[serde(with = "self")] SystemTime);

    #[test]
    fn test_serialize() {
        let now = SystemTime::now();
        let deadline = now + Duration::from_secs(10);
        let serialized_deadline = bincode::serialize(&AbsoluteToRelative(deadline)).unwrap();
        let deserialized_deadline: Duration = bincode::deserialize(&serialized_deadline).unwrap();
        // TODO: how to avoid flakiness?
        assert!(deserialized_deadline > Duration::from_secs(9));
    }

    #[test]
    fn test_deserialize() {
        let deadline = Duration::from_secs(10);
        let serialized_deadline = bincode::serialize(&deadline).unwrap();
        let AbsoluteToRelative(deserialized_deadline) =
            bincode::deserialize(&serialized_deadline).unwrap();
        // TODO: how to avoid flakiness?
        assert!(deserialized_deadline > SystemTime::now() + Duration::from_secs(9));
    }
}

assert_impl_all!(Context: Send, Sync);

fn ten_seconds_from_now() -> SystemTime {
    SystemTime::now() + Duration::from_secs(10)
}

/// Returns the context for the current request, or a default Context if no request is active.
pub fn current() -> Context {
    Context::current()
}

#[derive(Clone)]
struct Deadline(SystemTime);

impl Default for Deadline {
    fn default() -> Self {
        Self(ten_seconds_from_now())
    }
}

impl Context {
    /// Returns the context for the current request, or a default Context if no request is active.
    pub fn current() -> Self {
        let span = tracing::Span::current();
        Self {
            trace_context: trace::Context::try_from(&span)
                .unwrap_or_else(|_| trace::Context::default()),
            deadline: span
                .context()
                .get::<Deadline>()
                .cloned()
                .unwrap_or_default()
                .0,
        }
    }

    /// Returns the ID of the request-scoped trace.
    pub fn trace_id(&self) -> &TraceId {
        &self.trace_context.trace_id
    }
}

/// An extension trait for [`tracing::Span`] for propagating tarpc Contexts.
pub(crate) trait SpanExt {
    /// Sets the given context on this span. Newly-created spans will be children of the given
    /// context's trace context.
    fn set_context(&self, context: &Context);
}

impl SpanExt for tracing::Span {
    fn set_context(&self, context: &Context) {
        self.set_parent(
            opentelemetry::Context::new()
                .with_remote_span_context(opentelemetry::trace::SpanContext::new(
                    opentelemetry::trace::TraceId::from(context.trace_context.trace_id),
                    opentelemetry::trace::SpanId::from(context.trace_context.span_id),
                    opentelemetry::trace::TraceFlags::from(context.trace_context.sampling_decision),
                    true,
                    opentelemetry::trace::TraceState::default(),
                ))
                .with_value(Deadline(context.deadline)),
        );
    }
}
