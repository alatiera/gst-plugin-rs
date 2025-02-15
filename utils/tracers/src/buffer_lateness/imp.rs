// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-buffer-lateness:
 *
 * This tracer provides an easy way to collect lateness of each buffer when it is pushed out of a
 * pad in live pipelines.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS='buffer-lateness(file="/tmp/buffer_lateness.log")' gst-launch-1.0 audiotestsrc is-live=true ! queue ! fakesink
 * ```
 *
 * The generated file is a CSV file of the format
 *
 * ```csv
 * timestamp,element:pad name,pad pointer,buffer clock time,pipeline clock time,lateness,min latency
 * ```
 *
 * ## Parameters
 *
 * ### `file`
 *
 * Specifies the path to the file that will collect the CSV file with the buffer lateness.
 *
 * By default the file is written to `/tmp/buffer_lateness.log`.
 *
 * ### `include-filter`
 *
 * Specifies a regular expression for the `element:pad` names that should be included.
 *
 * By default this is not set.
 *
 * ### `exclude-filter`
 *
 * Specifies a regular expression for the `element:pad` names that should **not** be included.
 *
 * By default this is not set.
 */
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use regex::Regex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "buffer-lateness",
        gst::DebugColorFlags::empty(),
        Some("Tracer to collect buffer lateness"),
    )
});

#[derive(Debug)]
struct Settings {
    file: PathBuf,
    include_filter: Option<Regex>,
    exclude_filter: Option<Regex>,
}

impl Default for Settings {
    fn default() -> Self {
        let mut file = glib::tmp_dir();
        file.push("buffer_lateness.log");

        Self {
            file,
            include_filter: None,
            exclude_filter: None,
        }
    }
}

impl Settings {
    fn update_from_params(&mut self, imp: &BufferLateness, params: String) {
        let s = match gst::Structure::from_str(&format!("buffer-lateness,{}", params)) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, imp: imp, "failed to parse tracer parameters: {}", err);
                return;
            }
        };

        if let Ok(file) = s.get::<&str>("file") {
            gst::log!(CAT, imp: imp, "file= {}", file);
            self.file = PathBuf::from(file);
        }

        if let Ok(filter) = s.get::<&str>("include-filter") {
            gst::log!(CAT, imp: imp, "include filter= {}", filter);
            let filter = match Regex::new(filter) {
                Ok(filter) => Some(filter),
                Err(err) => {
                    gst::error!(
                        CAT,
                        imp: imp,
                        "Failed to compile include-filter regex: {}",
                        err
                    );
                    None
                }
            };
            self.include_filter = filter;
        }

        if let Ok(filter) = s.get::<&str>("exclude-filter") {
            gst::log!(CAT, imp: imp, "exclude filter= {}", filter);
            let filter = match Regex::new(filter) {
                Ok(filter) => Some(filter),
                Err(err) => {
                    gst::error!(
                        CAT,
                        imp: imp,
                        "Failed to compile exclude-filter regex: {}",
                        err
                    );
                    None
                }
            };
            self.exclude_filter = filter;
        }
    }
}

#[derive(Default)]
struct State {
    pads: HashMap<usize, Pad>,
    log: Vec<LogLine>,
    settings: Settings,
}

struct Pad {
    element_name: Option<Arc<glib::GString>>,
    pad_name: Arc<glib::GString>,
    latency: u64,
}

struct LogLine {
    timestamp: u64,
    element_name: Arc<glib::GString>,
    pad_name: Arc<glib::GString>,
    ptr: usize,
    buffer_clock_time: u64,
    pipeline_clock_time: u64,
    lateness: i64,
    min_latency: u64,
}

#[derive(Default)]
pub struct BufferLateness {
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for BufferLateness {
    const NAME: &'static str = "GstBufferLateness";
    type Type = super::BufferLateness;
    type ParentType = gst::Tracer;
}

impl ObjectImpl for BufferLateness {
    fn constructed(&self) {
        self.parent_constructed();

        if let Some(params) = self.obj().property::<Option<String>>("params") {
            let mut state = self.state.lock().unwrap();
            state.settings.update_from_params(self, params);
        }

        self.register_hook(TracerHook::ElementAddPad);
        self.register_hook(TracerHook::ElementRemovePad);
        self.register_hook(TracerHook::PadPushPre);
        self.register_hook(TracerHook::PadPushListPre);
        self.register_hook(TracerHook::PadQueryPost);
    }

    fn dispose(&self) {
        use std::io::prelude::*;

        let state = self.state.lock().unwrap();

        let mut file = match std::fs::File::create(&state.settings.file) {
            Ok(file) => file,
            Err(err) => {
                gst::error!(CAT, imp: self, "Failed to create file: {err}");
                return;
            }
        };

        gst::debug!(
            CAT,
            imp: self,
            "Writing file {}",
            state.settings.file.display()
        );

        for LogLine {
            timestamp,
            element_name,
            pad_name,
            ptr,
            buffer_clock_time,
            pipeline_clock_time,
            lateness,
            min_latency,
        } in &state.log
        {
            if let Err(err) = writeln!(&mut file, "{timestamp},{element_name}:{pad_name},0x{ptr:08x},{buffer_clock_time},{pipeline_clock_time},{lateness},{min_latency}") {
                gst::error!(CAT, imp: self, "Failed to write to file: {err}");
                return;
            }
        }
    }
}

impl GstObjectImpl for BufferLateness {}

impl TracerImpl for BufferLateness {
    fn element_add_pad(&self, _ts: u64, _element: &gst::Element, pad: &gst::Pad) {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }

        let ptr = pad.as_ptr() as usize;
        gst::debug!(
            CAT,
            imp: self,
            "new source pad: {} 0x{:08x}",
            pad.name(),
            ptr
        );

        let mut state = self.state.lock().unwrap();
        // FIXME: Element name might not be set yet here if the pad is added in instance_init
        // already.
        state.pads.entry(ptr).or_insert_with(|| Pad {
            element_name: None,
            pad_name: Arc::new(pad.name()),
            latency: 0,
        });
    }

    fn element_remove_pad(&self, _ts: u64, _element: &gst::Element, pad: &gst::Pad) {
        let ptr = pad.as_ptr() as usize;
        let mut state = self.state.lock().unwrap();
        state.pads.remove(&ptr);
    }

    fn pad_push_pre(&self, ts: u64, pad: &gst::Pad, buffer: &gst::Buffer) {
        let timestamp = match buffer.dts_or_pts() {
            Some(timestamp) => timestamp,
            None => return,
        };

        let element = match pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
            Some(element) => element,
            None => return,
        };

        let clock = match element.clock() {
            Some(clock) => clock,
            None => return,
        };

        let base_time = match element.base_time() {
            // FIXME: Workaround for base time being set to 0 initially instead of None
            Some(base_time)
                if base_time == gst::ClockTime::ZERO && element.start_time().is_some() =>
            {
                return
            }
            Some(base_time) => base_time,
            None => return,
        };

        let segment = match pad
            .sticky_event::<gst::event::Segment>(0)
            .map(|s| s.segment().clone())
            .and_then(|s| s.downcast::<gst::ClockTime>().ok())
        {
            Some(segment) => segment,
            None => return,
        };

        let ptr = pad.as_ptr() as usize;

        let mut state = self.state.lock().unwrap();
        let State {
            ref mut pads,
            ref mut log,
            ref settings,
            ..
        } = &mut *state;
        if let Some(pad) = pads.get_mut(&ptr) {
            if pad.element_name.is_none() {
                pad.element_name = Some(Arc::new(element.name()));

                let name = format!("{}:{}", pad.element_name.as_ref().unwrap(), pad.pad_name);
                if let Some(ref filter) = settings.include_filter {
                    if !filter.is_match(&name) {
                        pads.remove(&ptr);
                        return;
                    }
                }
                if let Some(ref filter) = settings.exclude_filter {
                    if filter.is_match(&name) {
                        pads.remove(&ptr);
                        return;
                    }
                }
            }

            let element_name = pad.element_name.as_ref().unwrap();

            let running_time = match segment.to_running_time(timestamp) {
                Some(running_time) => running_time,
                None => return,
            };

            let buffer_clock_time = running_time + base_time;
            let pipeline_clock_time = match clock.time() {
                Some(time) => time,
                None => return,
            };

            log.push(LogLine {
                timestamp: ts,
                element_name: element_name.clone(),
                pad_name: pad.pad_name.clone(),
                ptr,
                buffer_clock_time: buffer_clock_time.nseconds(),
                pipeline_clock_time: pipeline_clock_time.nseconds(),
                lateness: if buffer_clock_time > pipeline_clock_time {
                    -((buffer_clock_time.nseconds() - pipeline_clock_time.nseconds()) as i64)
                } else {
                    (pipeline_clock_time.nseconds() - buffer_clock_time.nseconds()) as i64
                },
                min_latency: pad.latency,
            });
        }
    }

    fn pad_push_list_pre(&self, ts: u64, pad: &gst::Pad, buffer_list: &gst::BufferList) {
        for buffer in buffer_list.iter_owned() {
            self.pad_push_pre(ts, pad, &buffer);
        }
    }

    #[allow(clippy::single_match)]
    fn pad_query_post(&self, _ts: u64, pad: &gst::Pad, query: &gst::QueryRef, res: bool) {
        if !res {
            return;
        }

        if pad.direction() != gst::PadDirection::Src {
            return;
        }

        match query.view() {
            gst::QueryView::Latency(l) => {
                let mut state = self.state.lock().unwrap();
                if let Some(pad) = state.pads.get_mut(&(pad.as_ptr() as usize)) {
                    let (live, min, _max) = l.result();
                    if live {
                        pad.latency = min.nseconds();
                    } else {
                        pad.latency = 0;
                    }
                }
            }
            _ => (),
        }
    }
}
