//! Plays a video or recording in the preview, from its moment, on GStreamer's playbin.
//!
//! GTK's own player (GstPlay) always builds playbin3, whose decodebin3 aborts the whole panel in GStreamer 1.28
//! (`mq_slot_handle_stream_start: assertion failed: (collection)`), when a stream starts after another one stopped as
//! much as when rows pass fast. playbin's decodebin is the older, steady one. A video's frames come through an appsink
//! and are drawn into the preview's picture, in place of its still.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::results::clock;

pub struct Player {
    playbin: gst::Element,
    playing: Cell<bool>,
    /// A slider and "1:23 / 25:10" under the picture or icon.
    pub controls: gtk::Box,
    /// The bus is watched, and the slider kept current, for as long as the player lives.
    watch: RefCell<Option<gst::bus::BusWatchGuard>>,
    timer: RefCell<Option<glib::SourceId>>,
}

/// A frame for the picture: width, height, bytes per row, and its RGBA pixels.
type Frame = (i32, i32, usize, glib::Bytes);

impl Player {
    /// Plays `path` from `at` seconds. A video's frames go to `picture`; without one, only the sound plays.
    pub fn start(path: &Path, at: f64, picture: Option<&gtk::Picture>) -> Option<Rc<Player>> {
        gst::init().ok()?;
        let uri = glib::filename_to_uri(path, None).ok()?;
        let playbin = gst::ElementFactory::make("playbin").property("uri", uri.as_str()).build().ok()?;
        let video_sink = match picture {
            Some(picture) => frames_into(picture)?,
            // A recording's cover art would otherwise open a window of its own.
            None => gst::ElementFactory::make("fakesink").build().ok()?,
        };
        playbin.set_property("video-sink", &video_sink);

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        slider.set_hexpand(true);
        slider.set_draw_value(false);
        let time = gtk::Label::new(None);
        time.add_css_class("showme-sub");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.append(&slider);
        controls.append(&time);
        let player = Rc::new(Player {
            playbin,
            playing: Cell::new(false),
            controls,
            watch: RefCell::new(None),
            timer: RefCell::new(None),
        });

        // Once the file is open (prerolled): to the moment, then play.
        let started = Cell::new(false);
        let weak = Rc::downgrade(&player);
        let watch = player
            .playbin
            .bus()?
            .add_watch_local(move |_, message| {
                let Some(player) = weak.upgrade() else { return glib::ControlFlow::Break };
                match message.view() {
                    gst::MessageView::AsyncDone(_) if !started.replace(true) => {
                        if at > 0.5 {
                            let _ = player.playbin.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                gst::ClockTime::from_nseconds((at * 1e9) as u64),
                            );
                        }
                        player.play();
                    }
                    gst::MessageView::Eos(_) => player.pause(),
                    gst::MessageView::Error(error) => {
                        eprintln!("showme-panel: can't play {}: {}", uri, error.error());
                        player.pause();
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .ok()?;
        *player.watch.borrow_mut() = Some(watch);

        // The slider follows the playing; dragging it moves there.
        let moving = Rc::new(Cell::new(false));
        let (weak, follow, shown) = (Rc::downgrade(&player), moving.clone(), slider.clone());
        let timer = glib::timeout_add_local(Duration::from_millis(250), move || {
            let Some(player) = weak.upgrade() else { return glib::ControlFlow::Break };
            let position = player.playbin.query_position::<gst::ClockTime>();
            let duration = player.playbin.query_duration::<gst::ClockTime>();
            if let (Some(position), Some(duration)) = (position, duration) {
                let (position, duration) = (position.seconds_f64(), duration.seconds_f64());
                follow.set(true);
                shown.set_range(0.0, duration.max(1.0));
                shown.set_value(position);
                follow.set(false);
                time.set_text(&format!("{} / {}", clock(position), clock(duration)));
            }
            glib::ControlFlow::Continue
        });
        *player.timer.borrow_mut() = Some(timer);
        let weak = Rc::downgrade(&player);
        slider.connect_value_changed(move |slider| {
            if moving.get() {
                return;
            }
            if let Some(player) = weak.upgrade() {
                let _ = player.playbin.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                    gst::ClockTime::from_nseconds((slider.value().max(0.0) * 1e9) as u64),
                );
            }
        });

        player.playbin.set_state(gst::State::Paused).ok()?;
        Some(player)
    }

    pub fn play(&self) {
        if self.playbin.set_state(gst::State::Playing).is_ok() {
            self.playing.set(true);
        }
    }

    pub fn pause(&self) {
        let _ = self.playbin.set_state(gst::State::Paused);
        self.playing.set(false);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.get()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

/// A sink that hands each frame, as RGBA, to `picture` on the main loop.
fn frames_into(picture: &gtk::Picture) -> Option<gst::Element> {
    let caps = gst_video::VideoCapsBuilder::new().format(gst_video::VideoFormat::Rgba).build();
    let sink = gst_app::AppSink::builder().caps(&caps).max_buffers(1).drop(true).build();
    let (frames, incoming) = async_channel::bounded::<Frame>(2);
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let info = sample
                    .caps()
                    .and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
                    .ok_or(gst::FlowError::Error)?;
                let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                let frame =
                    gst_video::VideoFrame::from_buffer_readable(buffer, &info).map_err(|_| gst::FlowError::Error)?;
                let stride = frame.plane_stride()[0] as usize;
                let pixels = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?.to_vec();
                // A frame the main loop hasn't drawn yet is simply replaced by a later one.
                let _ = frames.try_send((
                    info.width() as i32,
                    info.height() as i32,
                    stride,
                    glib::Bytes::from_owned(pixels),
                ));
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let picture = picture.downgrade();
    glib::spawn_future_local(async move {
        while let Ok((width, height, stride, pixels)) = incoming.recv().await {
            let Some(picture) = picture.upgrade() else { break };
            let texture = gdk::MemoryTexture::new(width, height, gdk::MemoryFormat::R8g8b8a8, &pixels, stride);
            picture.set_paintable(Some(&texture));
        }
    });
    // Decoders make many formats: a converter in front of the sink turns them into RGBA.
    let convert = gst::ElementFactory::make("videoconvert").build().ok()?;
    let bin = gst::Bin::new();
    bin.add_many([&convert, sink.upcast_ref()]).ok()?;
    convert.link(&sink).ok()?;
    let pad = gst::GhostPad::with_target(&convert.static_pad("sink")?).ok()?;
    bin.add_pad(&pad).ok()?;
    Some(bin.upcast())
}
