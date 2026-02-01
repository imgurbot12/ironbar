use std::sync::{Arc, Mutex};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::Context;
use libpulse_binding::context::introspect::SourceInfo;
use libpulse_binding::context::subscribe::Operation;
use libpulse_binding::def::SourceState;
use tokio::sync::broadcast;
use tracing::{debug, error, instrument, trace};

use super::{Client, Event, VolumeLevels};
use crate::channels::SyncSenderExt;
use crate::clients::volume::{ArcMutVec, ConnectionState};
use crate::lock;

#[derive(Debug, Clone)]
pub struct Source {
    index: u32,
    pub name: String,
    pub description: String,
    pub volume: VolumeLevels,
    pub muted: bool,
    pub active: bool,
}

impl From<&SourceInfo<'_>> for Source {
    fn from(value: &SourceInfo<'_>) -> Self {
        Self {
            index: value.index,
            name: value
                .name
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            description: value
                .description
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            muted: value.mute,
            volume: value.volume.into(),
            active: value.state == SourceState::Running,
        }
    }
}

impl Client {
    #[instrument(level = "trace")]
    pub fn sources(&self) -> ArcMutVec<Source> {
        self.data.sources.clone()
    }

    #[instrument(level = "trace")]
    pub fn set_default_source(&self, name: &str) {
        if let ConnectionState::Connected { context, .. } = &*lock!(self.connection) {
            lock!(context).set_default_source(name, |_| {});
        }
    }

    #[instrument(level = "trace")]
    pub fn set_source_volume(&self, name: &str, volume: f64) {
        if let ConnectionState::Connected { introspector, .. } = &mut *lock!(self.connection) {
            let Some(mut volume_levels) = ({
                let sources = self.sources();
                lock!(sources).iter().find_map(|s| {
                    if s.name == name {
                        Some(s.volume.clone())
                    } else {
                        None
                    }
                })
            }) else {
                return;
            };

            volume_levels.set_percent(volume);
            introspector.set_source_volume_by_name(name, &volume_levels.into(), None);
        }
    }

    #[instrument(level = "trace")]
    pub fn set_source_muted(&self, name: &str, muted: bool) {
        if let ConnectionState::Connected { introspector, .. } = &mut *lock!(self.connection) {
            introspector.set_source_mute_by_name(name, muted, None);
        }
    }
}

pub fn on_event(
    context: &Arc<Mutex<Context>>,
    sources: &ArcMutVec<Source>,
    default_source: &Arc<Mutex<Option<String>>>,
    tx: &broadcast::Sender<Event>,
    op: Operation,
    i: u32,
) {
    let introspect = lock!(context).introspect();

    match op {
        Operation::New => {
            debug!("new source");
            introspect.get_source_info_by_index(i, {
                let sources = sources.clone();
                let tx = tx.clone();

                move |info| add(info, &sources, &tx)
            });
        }
        Operation::Changed => {
            debug!("source changed");
            introspect.get_source_info_by_index(i, {
                let source = sources.clone();
                let default_source = default_source.clone();
                let tx = tx.clone();

                move |info| update(info, &source, &default_source, &tx)
            });
        }
        Operation::Removed => {
            debug!("source removed");
            remove(i, sources, tx);
        }
    }
}

pub fn add(
    info: ListResult<&SourceInfo>,
    sources: &ArcMutVec<Source>,
    tx: &broadcast::Sender<Event>,
) {
    let ListResult::Item(info) = info else {
        return;
    };

    trace!("adding {info:?}");
    lock!(sources).push(info.into());
    tx.send_expect(Event::AddSource(info.into()));
}

fn update(
    info: ListResult<&SourceInfo>,
    sources: &ArcMutVec<Source>,
    default_source: &Arc<Mutex<Option<String>>>,
    tx: &broadcast::Sender<Event>,
) {
    let ListResult::Item(info) = info else {
        return;
    };

    trace!("updating {info:?}");

    {
        let mut sources = lock!(sources);
        let Some(pos) = sources.iter().position(|source| source.index == info.index) else {
            error!("received update to untracked source input");
            return;
        };

        sources[pos] = info.into();

        // update in local copy
        if !sources[pos].active
            && let Some(default_source) = &*lock!(default_source)
        {
            sources[pos].active = &sources[pos].name == default_source;
        }
    }

    let mut source: Source = info.into();

    // update in broadcast copy
    if !source.active
        && let Some(default_source) = &*lock!(default_source)
    {
        source.active = &source.name == default_source;
    }

    tx.send_expect(Event::UpdateSource(source));
}

fn remove(index: u32, sources: &ArcMutVec<Source>, tx: &broadcast::Sender<Event>) {
    trace!("removing {index}");

    let mut sources = lock!(sources);

    if let Some(pos) = sources.iter().position(|s| s.index == index) {
        let info = sources.remove(pos);
        tx.send_expect(Event::RemoveSource(info.name));
    }
}
