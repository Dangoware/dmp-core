// Crate things
//use crate::music_controller::config::Config;
use crate::music_storage::library::URI;
use crossbeam_channel::unbounded;
use std::error::Error;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

// GStreamer things
use glib::FlagsClass;
use gst::{ClockTime, Element};
use gstreamer as gst;
use gstreamer::prelude::*;

// Extra things
use chrono::Duration;
use thiserror::Error;

const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug)]
pub enum PlayerCmd {
    Playing,
    Pause,
    Eos,
    AboutToFinish,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlayerState {
    Playing,
    Paused,
    Ready,
    Buffering(u8),
    Null,
    VoidPending,
}

impl From<gst::State> for PlayerState {
    fn from(value: gst::State) -> Self {
        match value {
            gst::State::VoidPending => Self::VoidPending,
            gst::State::Playing => Self::Playing,
            gst::State::Paused => Self::Paused,
            gst::State::Ready => Self::Ready,
            gst::State::Null => Self::Null,
        }
    }
}

impl TryInto<gst::State> for PlayerState {
    fn try_into(self) -> Result<gst::State, Box<dyn Error>> {
        match self {
            Self::VoidPending => Ok(gst::State::VoidPending),
            Self::Playing => Ok(gst::State::Playing),
            Self::Paused => Ok(gst::State::Paused),
            Self::Ready => Ok(gst::State::Ready),
            Self::Null => Ok(gst::State::Null),
            state => Err(format!("Invalid gst::State: {:?}", state).into()),
        }
    }

    type Error = Box<dyn Error>;
}

#[derive(Error, Debug)]
pub enum PlayerError {
    #[error("player initialization failed")]
    Init(#[from] glib::Error),
    #[error("element factory failed to create playbin3")]
    Factory(#[from] glib::BoolError),
    #[error("could not change playback state")]
    StateChange(#[from] gst::StateChangeError),
    #[error("failed to build gstreamer item")]
    Build,
    #[error("poison error")]
    Poison,
    #[error("general player error")]
    General,
}

#[derive(Debug, PartialEq, Eq)]
enum MonitorCmd {
    Idle,

    /// The song is currently switching
    Switching,

    /// A new thing has been loaded
    Loaded {
        start: Duration,
        end:   Duration,
    },

    /// Causes the thread to send the current time
    PlaybackTime,

    /// When this is sent, the monitoring thread will die!
    Finished
}

/// An instance of a music player with a GStreamer backend
pub struct Player {
    source:     Option<URI>,
    pub play_state_rx: crossbeam::channel::Receiver<PlayerCmd>,
    position_rx: crossbeam::channel::Receiver<Option<Duration>>,
    monitor_tx: crossbeam::channel::Sender<MonitorCmd>,
    playbin:    Arc<RwLock<Element>>,
    volume:     f64,
    start:      Option<Duration>,
    end:        Option<Duration>,
    paused:     bool,
}

impl Player {
    pub fn new() -> Result<Self, PlayerError> {
        // Initialize GStreamer, maybe figure out how to nicely fail here
        gst::init()?;
        let ctx = glib::MainContext::default();
        let _guard = ctx.acquire();
        let mainloop = glib::MainLoop::new(Some(&ctx), false);

        let playbin_arc = Arc::new(RwLock::new(
            gst::ElementFactory::make("playbin3").build()?,
        ));

        let playbin = playbin_arc.clone();

        let flags = playbin.read().unwrap().property_value("flags");
        let flags_class = FlagsClass::with_type(flags.type_()).unwrap();

        // Set up the Playbin flags to only play audio
        let flags = flags_class
            .builder_with_value(flags)
            .ok_or(PlayerError::Build)?
            .set_by_nick("audio")
            .set_by_nick("download")
            .unset_by_nick("video")
            .unset_by_nick("text")
            .build()
            .ok_or(PlayerError::Build)?;

        playbin.write().unwrap().set_property_from_value("flags", &flags);
        playbin.write().unwrap().set_property("instant-uri", true);

        // This thread monitors the playback position of the playbin3
        let (play_state_tx, play_state_rx) = unbounded::<PlayerCmd>();
        let (position_tx, position_rx)  = unbounded::<Option<Duration>>();
        let (monitor_tx, monitor_rx)    = unbounded::<MonitorCmd>();
        let _playback_monitor = std::thread::spawn(move || { //TODO: Figure out how to return errors nicely in threads
            let mut start_time: Option<Duration> = None;
            let mut end_time: Option<Duration> = None;
            let mut switching = false;
            loop {
                // Check for new messages or updates about how to proceed
                let message = match monitor_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(mes) => Some(mes),
                    Err(_) => None,
                };

                // Get the updated position from playbin
                let mut pos_temp = playbin_arc
                    .read()
                    .unwrap()
                    .query_position::<ClockTime>()
                    .map(|pos| Duration::nanoseconds(pos.nseconds() as i64));

                if !switching
                    && pos_temp.is_some()
                    && start_time.is_some()
                    && end_time.is_some()
                {
                    // Check if the current playback position is close to the end
                    let finish_point = end_time.unwrap() - Duration::milliseconds(250);
                    if pos_temp.unwrap() >= end_time.unwrap() {
                        let _ = play_state_tx.try_send(PlayerCmd::Eos);
                        playbin_arc
                            .write()
                            .unwrap()
                            .set_state(gst::State::Ready)
                            .expect("Unable to set the pipeline state");
                    } else if pos_temp.unwrap() >= finish_point {
                        let _ = play_state_tx.try_send(PlayerCmd::AboutToFinish);
                    }

                    // This has to be done AFTER the current time in the file
                    // is calculated, or everything else is wrong
                    pos_temp = Some(pos_temp.unwrap() - start_time.unwrap())
                }

                match message {
                    // A new start and end time were provided, use them
                    Some(MonitorCmd::Loaded{start, end}) => {
                        start_time  = Some(start);
                        end_time    = Some(end);
                        switching   = false;
                    },

                    // Exit the loop immediately, terminating the thread
                    Some(MonitorCmd::Finished) => {
                        break
                    },

                    // The player is doing nothing
                    Some(MonitorCmd::Idle) => {
                        start_time = None;
                        end_time = None;
                        switching = false;
                    },

                    // The player isn't playing right now, don't try to update!!
                    Some(MonitorCmd::Switching) => {
                        start_time = None;
                        end_time = None;
                        switching = true;
                    },

                    // Return the playback time immediately through the channel
                    Some(MonitorCmd::PlaybackTime) => {
                        let _ = position_tx.try_send(pos_temp);
                    },
                    _ => ()
                }

                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        // Set up the thread to monitor bus messages
        let playbin_bus_ctrl = Arc::clone(&playbin);
        let bus_watch = playbin
            .read()
            .unwrap()
            .bus()
            .expect("Failed to get GStreamer message bus")
            .add_watch(move |_bus, msg| {
                match msg.view() {
                    gst::MessageView::Eos(_) => (),
                    gst::MessageView::StreamStart(_) => (),
                    gst::MessageView::Error(_) => {
                        playbin_bus_ctrl
                            .write()
                            .unwrap()
                            .set_state(gst::State::Ready)
                            .unwrap();

                        playbin_bus_ctrl
                            .write()
                            .unwrap()
                            .set_state(gst::State::Playing)
                            .unwrap();
                    }
                    /* TODO: Fix buffering!!
                    gst::MessageView::Buffering(buffering) => {
                        let percent = buffering.percent();
                        if percent < 100 {
                            playbin_bus_ctrl
                                .write()
                                .unwrap()
                                .set_state(gst::State::Paused)
                                .unwrap();
                        } else if !(buffering) {
                            playbin_bus_ctrl
                                .write()
                                .unwrap()
                                .set_state(gst::State::Playing)
                                .unwrap();
                        }
                    }
                    */
                    _ => (),
                }
                glib::ControlFlow::Continue
            })
            .expect("Failed to connect to GStreamer message bus");

        // Set up a thread to watch the messages
        std::thread::spawn(move || {
            let _watch = bus_watch;
            mainloop.run()
        });

        let source = None;
        Ok(Self {
            source,
            playbin,
            play_state_rx,
            position_rx,
            monitor_tx,
            volume: 1.0,
            start: None,
            end: None,
            paused: false,
        })
    }

    pub fn source(&self) -> &Option<URI> {
        &self.source
    }

    pub fn enqueue_next(&mut self, next_track: &URI) -> Result<(), Box<dyn Error>> {
        self.set_source(next_track)
    }

    /// Set the playback URI
    fn set_source(&mut self, source: &URI) -> Result<(), Box<dyn Error>> {
        // Make sure the playback tracker knows the stuff is stopped
        self.monitor_tx.send(MonitorCmd::Switching).unwrap();

        let uri = self.playbin.read().unwrap().property_value("current-uri");
        self.source = Some(source.clone());
        match source {
            URI::Cue { start, end, .. } => {
                self.playbin
                    .write()
                    .unwrap()
                    .set_property("uri", source.as_uri());

                // Set the start and end positions of the CUE file
                self.start = Some(Duration::from_std(*start).unwrap());
                self.end = Some(Duration::from_std(*end).unwrap());

                // Send the updated position to the tracker
                self.monitor_tx.send(MonitorCmd::Loaded {
                    start: self.start.unwrap(),
                    end: self.end.unwrap()
                }).unwrap();

                // Wait for it to be ready, and then move to the proper position
                self.play().unwrap();
                let now = std::time::Instant::now();
                while now.elapsed() < std::time::Duration::from_millis(20) {
                    if self.seek_to(Duration::from_std(*start).unwrap()).is_ok() {
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                panic!("Couldn't seek to beginning of cue track in reasonable time (>20ms)");
            }
            _ => {
                self.playbin
                    .write()
                    .unwrap()
                    .set_property("uri", source.as_uri());

                self.play().unwrap();

                self.monitor_tx.send(MonitorCmd::Idle)?;

                while uri.get::<&str>().unwrap_or("")
                    == self.property("current-uri").get::<&str>().unwrap_or("")
                    || self.raw_position().is_none()
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }

                self.start = Some(Duration::seconds(0));
                self.end = self.raw_duration();

                // Send the updated position to the tracker
                self.monitor_tx.send(MonitorCmd::Loaded {
                    start: self.start.unwrap(),
                    end: self.end.unwrap()
                }).unwrap();
            }
        }
        Ok(())
    }

    /// Gets a mutable reference to the playbin element
    fn playbin_mut(
        &mut self,
    ) -> Result<RwLockWriteGuard<gst::Element>, std::sync::PoisonError<RwLockWriteGuard<'_, Element>>>
    {
        let element = match self.playbin.write() {
            Ok(element) => element,
            Err(err) => return Err(err),
        };
        Ok(element)
    }

    /// Gets a read-only reference to the playbin element
    fn playbin(
        &self,
    ) -> Result<RwLockReadGuard<gst::Element>, std::sync::PoisonError<RwLockReadGuard<'_, Element>>>
    {
        let element = match self.playbin.read() {
            Ok(element) => element,
            Err(err) => return Err(err),
        };
        Ok(element)
    }

    /// Set the playback volume, accepts a float from 0 to 1
    pub fn set_volume(&mut self, volume: f64) {
        self.volume = volume.clamp(0.0, 1.0);
        self.set_gstreamer_volume(self.volume);
    }

    /// Set volume of the internal playbin player, can be
    /// used to bypass the main volume control for seeking
    fn set_gstreamer_volume(&mut self, volume: f64) {
        self.playbin_mut().unwrap().set_property("volume", volume)
    }

    /// Returns the current volume level, a float from 0 to 1
    pub fn volume(&mut self) -> f64 {
        self.volume
    }

    fn set_state(&mut self, state: gst::State) -> Result<(), gst::StateChangeError> {
        self.playbin_mut().unwrap().set_state(state)?;

        Ok(())
    }

    pub fn ready(&mut self) -> Result<(), gst::StateChangeError> {
        self.set_state(gst::State::Ready)
    }

    /// If the player is paused or stopped, starts playback
    pub fn play(&mut self) -> Result<(), gst::StateChangeError> {
        self.set_state(gst::State::Playing)
    }

    /// Pause, if playing
    pub fn pause(&mut self) -> Result<(), gst::StateChangeError> {
        //*self.paused.write().unwrap() = true;
        self.set_state(gst::State::Paused)
    }

    /// Resume from being paused
    pub fn resume(&mut self) -> Result<(), gst::StateChangeError> {
        //*self.paused.write().unwrap() = false;
        self.set_state(gst::State::Playing)
    }

    /// Check if playback is paused
    pub fn is_paused(&mut self) -> bool {
        self.playbin().unwrap().current_state() == gst::State::Paused
    }

    /// Get the current playback position of the player
    pub fn position(&mut self) -> Result<Option<Duration>, Box<dyn Error>> {
        self.monitor_tx.send(MonitorCmd::PlaybackTime)?;

        match self.position_rx.recv_timeout(DEFAULT_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(err) => Err(err.into()),
        }
    }

    pub fn raw_position(&self) -> Option<Duration> {
        self.playbin
            .read()
            .unwrap()
            .query_position::<ClockTime>()
            .map(|pos| Duration::nanoseconds(pos.nseconds() as i64))
    }

    /// Get the duration of the currently playing track
    pub fn duration(&mut self) -> Option<Duration> {
        if self.end.is_some() && self.start.is_some() {
            Some(self.end.unwrap() - self.start.unwrap())
        } else {
            self.raw_duration()
        }
    }

    pub fn raw_duration(&self) -> Option<Duration> {
        self.playbin()
            .unwrap()
            .query_duration::<ClockTime>()
            .map(|pos| Duration::nanoseconds(pos.nseconds() as i64))
    }

    /// Seek relative to the current position
    pub fn seek_by(&mut self, seek_amount: Duration) -> Result<(), Box<dyn Error>> {
        let pos = self.position()?;

        let time_pos = match pos {
            Some(pos) => pos,
            None => return Err("No position".into()),
        };
        let seek_pos = time_pos + seek_amount;

        self.seek_to(seek_pos)?;
        Ok(())
    }

    /// Seek absolutely
    pub fn seek_to(&mut self, target_pos: Duration) -> Result<(), Box<dyn Error>> {
        let start = if self.start.is_none() {
            return Err("Failed to seek: No START time".into());
        } else {
            self.start.unwrap()
        };

        let end = if self.end.is_none() {
            return Err("Failed to seek: No END time".into());
        } else {
            self.end.unwrap()
        };

        let adjusted_target = target_pos + start;
        let clamped_target = adjusted_target.clamp(start, end);

        let seek_pos_clock =
            ClockTime::from_useconds(clamped_target.num_microseconds().unwrap() as u64);

        self.set_gstreamer_volume(0.0);
        self.playbin_mut()
            .unwrap()
            .seek_simple(gst::SeekFlags::FLUSH, seek_pos_clock)?;
        self.set_gstreamer_volume(self.volume);
        Ok(())
    }

    /// Get the current state of the playback
    pub fn state(&mut self) -> PlayerState {
        self.playbin().unwrap().current_state().into()
        /*
        match *self.buffer.read().unwrap() {
            None => self.playbin().unwrap().current_state().into(),
            Some(value) => PlayerState::Buffering(value),
        }
        */
    }

    pub fn property(&self, property: &str) -> glib::Value {
        self.playbin().unwrap().property_value(property)
    }

    /// Stop the playback entirely
    pub fn stop(&mut self) -> Result<(), gst::StateChangeError> {
        self.pause()?;
        self.ready()?;

        // Send the updated position to the tracker
        self.monitor_tx.send(MonitorCmd::Idle).unwrap();

        // Set all positions to none
        self.start = None;
        self.end = None;
        Ok(())
    }
}

impl Drop for Player {
    /// Cleans up the `GStreamer` pipeline and the monitoring
    /// thread when [Player] is dropped.
    fn drop(&mut self) {
        self.playbin_mut()
            .unwrap()
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");
        let _ = self.monitor_tx.send(MonitorCmd::Finished);
    }
}
