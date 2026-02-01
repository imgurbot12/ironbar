use std::sync::{Arc, Mutex};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::Context;
use libpulse_binding::context::introspect::SourceOutputInfo;
use libpulse_binding::context::subscribe::Operation;
use tokio::sync::broadcast;
use tracing::{debug, error, instrument, trace};

use super::{ArcMutVec, Client, ConnectionState, Event, VolumeLevels};
use crate::channels::SyncSenderExt;
use crate::lock;

#[derive(Debug, Clone)]
pub struct SourceOutput {
    pub index: u32,
    pub name: String,
    pub volume: VolumeLevels,
    pub muted: bool,

    pub can_set_volume: bool,
}

impl From<&SourceOutputInfo<'_>> for SourceOutput {
    fn from(value: &SourceOutputInfo) -> Self {
        Self {
            index: value.index,
            name: value
                .name
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            muted: value.mute,
            volume: value.volume.into(),
            can_set_volume: value.has_volume && value.volume_writable,
        }
    }
}

impl Client {
    #[instrument(level = "trace")]
    pub fn source_outputs(&self) -> ArcMutVec<SourceOutput> {
        self.data.source_outputs.clone()
    }

    #[instrument(level = "trace")]
    pub fn set_output_volume(&self, index: u32, volume_percent: f64) {
        if let ConnectionState::Connected { introspector, .. } = &mut *lock!(self.connection) {
            let Some(mut volume_levels) = ({
                let outputs = self.source_outputs();
                lock!(outputs).iter().find_map(|s| {
                    if s.index == index {
                        Some(s.volume.clone())
                    } else {
                        None
                    }
                })
            }) else {
                return;
            };

            volume_levels.set_percent(volume_percent);
            introspector.set_source_output_volume(index, &volume_levels.into(), None);
        }
    }

    #[instrument(level = "trace")]
    pub fn set_output_muted(&self, index: u32, muted: bool) {
        if let ConnectionState::Connected { introspector, .. } = &mut *lock!(self.connection) {
            introspector.set_source_output_mute(index, muted, None);
        }
    }
}

pub fn on_event(
    context: &Arc<Mutex<Context>>,
    outputs: &ArcMutVec<SourceOutput>,
    tx: &broadcast::Sender<Event>,
    op: Operation,
    i: u32,
) {
    let introspect = lock!(context).introspect();

    match op {
        Operation::New => {
            debug!("new source output");
            introspect.get_source_output_info(i, {
                let outputs = outputs.clone();
                let tx = tx.clone();

                move |info| add(info, &outputs, &tx)
            });
        }
        Operation::Changed => {
            debug!("source output changed");
            introspect.get_source_output_info(i, {
                let outputs = outputs.clone();
                let tx = tx.clone();

                move |info| update(info, &outputs, &tx)
            });
        }
        Operation::Removed => {
            debug!("source output removed");
            remove(i, outputs, tx);
        }
    }
}

pub fn add(
    info: ListResult<&SourceOutputInfo>,
    outputs: &ArcMutVec<SourceOutput>,
    tx: &broadcast::Sender<Event>,
) {
    let ListResult::Item(info) = info else {
        return;
    };

    trace!("adding {info:?}");

    lock!(outputs).push(info.into());
    tx.send_expect(Event::AddOutput(info.into()));
}

fn update(
    info: ListResult<&SourceOutputInfo>,
    outputs: &ArcMutVec<SourceOutput>,
    tx: &broadcast::Sender<Event>,
) {
    let ListResult::Item(info) = info else {
        return;
    };

    trace!("updating {info:?}");

    let output_info: SourceOutput = info.into();

    {
        let mut outputs = lock!(outputs);
        if let Some(pos) = outputs
            .iter()
            .position(|output| output.index == output_info.index)
        {
            outputs[pos] = output_info.clone();
        } else {
            error!("received update to untracked source output");
            return;
        }
    }

    tx.send_expect(Event::UpdateOutput(output_info));
}

fn remove(index: u32, outputs: &ArcMutVec<SourceOutput>, tx: &broadcast::Sender<Event>) {
    let mut outputs = lock!(outputs);

    trace!("removing {index}");

    if let Some(pos) = outputs.iter().position(|s| s.index == index) {
        let info = outputs.remove(pos);
        tx.send_expect(Event::RemoveOutput(info.index));
    }
}
