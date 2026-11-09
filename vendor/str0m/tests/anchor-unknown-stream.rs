//! Anchor fix 3 (.claude/docs/dependencies.md): data on a stream with no channel is dropped
//! with a debug log, not a warn the remote could use to flood the log. Fails on the published
//! 0.24.1.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use str0m::channel::ChannelConfig;
use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

mod common;
use common::{connect_l_r, init_crypto_default, progress};

/// The levels of the events that report a dropped ChannelData.
#[derive(Clone, Default)]
struct DropLevels(Arc<Mutex<Vec<Level>>>);

impl<S: Subscriber> Layer<S> for DropLevels {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut message = Message::default();
        event.record(&mut message);
        if message.0.starts_with("Drop ChannelData") {
            self.0.lock().unwrap().push(*event.metadata().level());
        }
    }
}

/// The formatted message of an event.
#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

#[test]
fn data_on_a_stream_without_a_channel_is_dropped_at_debug() {
    init_crypto_default();
    let levels = DropLevels::default();
    let subscriber = tracing_subscriber::registry().with(levels.clone());
    tracing::subscriber::with_default(subscriber, || {
        let (mut l, mut r) = connect_l_r();
        // A negotiated channel only L knows: R receives data on a stream it has no channel for.
        let cid = l.direct_api().create_data_channel(ChannelConfig {
            negotiated: Some(7),
            label: "unknown".into(),
            ..Default::default()
        });
        let end = l.duration() + Duration::from_secs(5);
        while l.duration() < end {
            if let Some(mut chan) = l.channel(cid) {
                chan.write(true, b"x").expect("to write");
            }
            progress(&mut l, &mut r).expect("clean progress");
        }
    });
    let levels = levels.0.lock().unwrap();
    assert!(!levels.is_empty(), "no ChannelData was dropped");
    assert!(levels.iter().all(|level| *level == Level::DEBUG), "{levels:?}");
}
