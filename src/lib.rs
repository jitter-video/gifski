/*

 gifski pngquant-based GIF encoder
 © 2017 Kornel Lesiński

 This program is free software: you can redistribute it and/or modify
 it under the terms of the GNU Affero General Public License as
 published by the Free Software Foundation, either version 3 of the
 License, or (at your option) any later version.

 This program is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 GNU Affero General Public License for more details.

 You should have received a copy of the GNU Affero General Public License
 along with this program.  If not, see <https://www.gnu.org/licenses/>.
*/
#![doc(html_logo_url = "https://gif.ski/icon.png")]
mod denoise;
mod encoderust;
mod error;
mod ordqueue;

use encoderust::RustEncoder;
use imagequant::{Attributes, Image, QuantizationResult};
use imgref::*;
use rgb::*;

pub use imgref::ImgVec;
pub use rgb::FromSlice;

use crate::denoise::*;
pub use crate::error::*;
use crate::ordqueue::{OrdQueue, OrdQueueIter};

use crossbeam_channel::{Receiver, Sender};
use std::io::prelude::*;

pub struct InputFrameUnresized {
    /// The pixels to resize and encode
    frame: ImgVec<RGBA8>,
    /// Time in seconds when to display the frame. First frame should start at 0.
    presentation_timestamp: f64,
    frame_index: usize,
}

struct InputFrame {
    /// The pixels to encode
    frame: ImgVec<RGBA8>,
    /// The same as above, but with smart blur applied (for denoiser)
    frame_blurred: ImgVec<RGB8>,
    /// Time in seconds when to display the frame. First frame should start at 0.
    presentation_timestamp: f64,
}

/// Number of repetitions
pub type Repeat = gif::Repeat;

/// Encoding settings for the `new()` function
#[derive(Copy, Clone, Debug)]
pub struct Settings {
    /// Resize to max this width if non-0.
    pub width: Option<u32>,
    /// Resize to max this height if width is non-0. Note that aspect ratio is not preserved.
    pub height: Option<u32>,
    /// 1-100, but useful range is 50-100. Recommended to set to 100.
    pub quality: u8,
    /// Lower quality, but faster encode.
    pub fast: bool,
    /// Sets the looping method for the image sequence.
    pub repeat: Repeat,
    /// Maximum queue size
    pub max_queue_size: usize,
}

#[derive(Copy, Clone)]
#[non_exhaustive]
struct SettingsExt {
    pub s: Settings,
    pub extra_effort: bool,
    pub motion_quality: u8,
    pub giflossy_quality: u8,
}

impl Settings {
    /// quality is used in other places, like gifsicle or frame differences,
    /// and it's better to lower quality there before ruining quantization
    pub(crate) fn color_quality(&self) -> u8 {
        (u16::from(self.quality) * 4 / 3).min(100) as u8
    }

    /// `add_frame` is going to resize the images to this size.
    #[must_use]
    pub fn dimensions_for_image(&self, width: usize, height: usize) -> (usize, usize) {
        dimensions_for_image((width, height), (self.width, self.height))
    }
}

impl SettingsExt {
    pub(crate) fn gifsicle_loss(&self) -> u32 {
        0
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            quality: 100,
            fast: false,
            repeat: Repeat::Infinite,
            max_queue_size: 5,
        }
    }
}

/// Collect frames that will be encoded
///
/// Note that writing will finish only when the collector is dropped.
/// Collect frames on another thread, or call `drop(collector)` before calling `writer.write()`!
pub struct Collector {
    pub queue: Sender<InputFrameUnresized>,
}

/// Perform GIF writing
pub struct Writer {
    /// Input frame decoder results
    queue_iter: Option<Receiver<InputFrameUnresized>>,
    settings: SettingsExt,
}

struct GIFFrame {
    left: u16,
    top: u16,
    image: ImgVec<u8>,
    pal: Vec<RGBA8>,
    dispose: gif::DisposalMethod,
    transparent_index: Option<u8>,
}

/// Frame before quantization
struct DiffMessage {
    /// 1..
    ordinal_frame_number: usize,
    pts: f64,
    frame_duration: f64,
    image: ImgVec<RGBA8>,
    importance_map: Vec<u8>,
}

/// Frame post quantization, before remap
struct RemapMessage {
    /// 1..
    ordinal_frame_number: usize,
    end_pts: f64,
    dispose: gif::DisposalMethod,
    liq: Attributes,
    remap: QuantizationResult,
    liq_image: Image<'static>,
    out_buf: Vec<u8>,
}

/// Frame post quantization and remap
struct FrameMessage {
    /// 1..
    ordinal_frame_number: usize,
    end_pts: f64,
    frame: GIFFrame,
    screen_width: u16,
    screen_height: u16,
}

/// Start new encoding
///
/// Encoding is multi-threaded, and the `Collector` and `Writer`
/// can be used on sepate threads.
///
/// You feed input frames to the `Collector`, and ask the `Writer` to
/// start writing the GIF.
#[inline]
pub fn new(settings: Settings) -> CatResult<(Collector, Writer)> {
    if settings.quality == 0 || settings.quality > 100 {
        return Err(Error::WrongSize("quality must be 1-100".into())); // I forgot to add a better error variant
    }
    if settings.width.unwrap_or(0) > 1 << 16 || settings.height.unwrap_or(0) > 1 << 16 {
        return Err(Error::WrongSize("image size too large".into()));
    }

    let (queue, queue_iter) = crossbeam_channel::bounded(settings.max_queue_size); // should be sufficient for denoiser lookahead
    Ok((
        Collector { queue },
        Writer {
            queue_iter: Some(queue_iter),
            settings: SettingsExt {
                motion_quality: settings.quality,
                giflossy_quality: settings.quality,
                extra_effort: false,
                s: settings,
            },
        },
    ))
}

impl Collector {
    /// Frame index starts at 0.
    ///
    /// Set each frame (index) only once, but you can set them in any order.
    ///
    /// Presentation timestamp is time in seconds (since file start at 0) when this frame is to be displayed.
    ///
    /// If the first frame doesn't start at pts=0, the delay will be used for the last frame.
    pub fn add_frame_rgba(
        &self,
        frame_index: usize,
        frame: ImgVec<RGBA8>,
        presentation_timestamp: f64,
    ) -> CatResult<()> {
        self.queue.send(InputFrameUnresized {
            frame_index,
            frame,
            presentation_timestamp,
        })?;
        Ok(())
    }
}

/// `add_frame` is going to resize the image to this size.
/// The `Option` args are user-specified max width and max height
fn dimensions_for_image(
    (img_w, img_h): (usize, usize),
    resize_to: (Option<u32>, Option<u32>),
) -> (usize, usize) {
    match resize_to {
        (None, None) => {
            let factor = ((img_w * img_h + 800 * 600 / 2) as f64 / f64::from(800 * 600))
                .sqrt()
                .round() as usize;
            if factor > 1 {
                (img_w / factor, img_h / factor)
            } else {
                (img_w, img_h)
            }
        }
        (Some(w), Some(h)) => ((w as usize).min(img_w), (h as usize).min(img_h)),
        (Some(w), None) => {
            let w = (w as usize).min(img_w);
            (w, img_h * w / img_w)
        }
        (None, Some(h)) => {
            let h = (h as usize).min(img_h);
            (img_w * h / img_h, h)
        }
    }
}

#[derive(Copy, Clone)]
enum LastFrameDuration {
    FixedOffset(f64),
    FrameRate(f64),
}

impl LastFrameDuration {
    pub fn value(&self) -> f64 {
        match self {
            Self::FixedOffset(val) | Self::FrameRate(val) => *val,
        }
    }

    pub fn shift_every_pts_by(&self) -> f64 {
        match self {
            Self::FixedOffset(offset) => *offset,
            Self::FrameRate(_) => 0.,
        }
    }
}

/// Encode collected frames
impl Writer {
    #[deprecated(note = "please don't use, it will be in Settings eventually")]
    #[doc(hidden)]
    pub fn set_extra_effort(&mut self, enabled: bool) {
        self.settings.extra_effort = enabled;
    }

    #[deprecated(note = "please don't use, it will be in Settings eventually")]
    #[doc(hidden)]
    pub fn set_motion_quality(&mut self, q: u8) {
        self.settings.motion_quality = q;
    }

    #[deprecated(note = "please don't use, it will be in Settings eventually")]
    #[doc(hidden)]
    pub fn set_lossy_quality(&mut self, q: u8) {
        self.settings.giflossy_quality = q;
    }

    /// `importance_map` is computed from previous and next frame.
    /// Improves quality of pixels visible for longer.
    /// Avoids wasting palette on pixels identical to the background.
    ///
    /// `background` is the previous frame.
    fn quantize(
        image: ImgVec<RGBA8>,
        importance_map: &[u8],
        first_frame: bool,
        needs_transparency: bool,
        prev_frame_keeps: bool,
        settings: &SettingsExt,
    ) -> CatResult<(Attributes, QuantizationResult, Image<'static>, Vec<u8>)> {
        let mut liq = Attributes::new();
        if settings.s.fast {
            liq.set_speed(10)?;
        } else if settings.extra_effort {
            liq.set_speed(1)?;
        }
        let quality = if !first_frame {
            settings.s.color_quality()
        } else {
            100 // the first frame is too important to ruin it
        };
        liq.set_quality(0, quality)?;
        if settings.s.quality < 50 {
            liq.set_max_colors(
                u32::from(settings.s.quality * 2)
                    .max(16)
                    .next_power_of_two(),
            )?;
        }
        let (buf, width, height) = image.into_contiguous_buf();
        let mut img = liq.new_image(buf, width, height, 0.)?;
        // only later remapping tracks which area has been damaged by transparency
        // so for previous-transparent background frame the importance map may be invalid
        // because there's a transparent hole in the background not taken into account,
        // and palette may lack colors to fill that hole
        if first_frame || prev_frame_keeps {
            img.set_importance_map(importance_map)?;
        }
        // first frame may be transparent too, so it's not just for diffs
        if needs_transparency {
            img.add_fixed_color(RGBA8::new(0, 0, 0, 0))?;
        }

        let mut res = liq.quantize(&mut img)?;
        res.set_dithering_level((f32::from(settings.s.quality) / 50.0 - 1.).max(0.2))?;

        let mut out = Vec::new();
        out.try_reserve_exact(width * height)
            .map_err(imagequant::liq_error::from)?;
        res.optionally_prepare_for_dithering_with_background_set(
            &mut img,
            &mut out.spare_capacity_mut()[..width * height],
        )?;

        Ok((liq, res, img, out))
    }

    fn remap<'a>(
        liq: Attributes,
        mut res: QuantizationResult,
        mut img: Image<'a>,
        background: Option<ImgRef<'a, RGBA8>>,
        mut pal_img: Vec<u8>,
    ) -> CatResult<(ImgVec<u8>, Vec<RGBA8>)> {
        if let Some(bg) = background {
            img.set_background(Image::new_stride_borrowed(
                &liq,
                bg.buf(),
                bg.width(),
                bg.height(),
                bg.stride(),
                0.,
            )?)?;
        }

        let pal = res.remap_into_vec(&mut img, &mut pal_img)?;
        debug_assert_eq!(img.width() * img.height(), pal_img.len());

        Ok((Img::new(pal_img, img.width(), img.height()), pal))
    }

    fn write_frames(
        write_queue: Receiver<FrameMessage>,
        writer: &mut dyn Write,
        settings: &SettingsExt,
    ) -> CatResult<()> {
        let mut pts_in_delay_units = 0_u64;

        let mut enc = RustEncoder::new(writer);

        let mut n_done = 0;
        for FrameMessage {
            frame,
            ordinal_frame_number,
            end_pts,
            screen_width,
            screen_height,
            ..
        } in write_queue
        {
            let frame = RustEncoder::<&mut dyn std::io::Write>::compress_frame(frame, settings)?;

            let end_pts = end_pts as f64;
            let delay = ((end_pts * 100_f64).round() as u64)
                .saturating_sub(pts_in_delay_units)
                .min(30000) as u16;
            pts_in_delay_units += u64::from(delay);

            debug_assert_ne!(0, delay);

            // skip frames with bad pts
            if delay != 0 {
                enc.write_frame(frame, delay, screen_width, screen_height, &settings.s)?;
            }

            // loop to report skipped frames too
            while n_done < ordinal_frame_number {
                n_done += 1;
            }
        }
        if n_done == 0 {
            return Err(Error::NoFrames);
        }

        Ok(())
    }

    /// Start writing frames. This function will not return until `Collector` is dropped.
    ///
    /// `outfile` can be any writer, such as `File` or `&mut Vec`.
    ///
    /// `ProgressReporter.increase()` is called each time a new frame is being written.
    #[allow(unused_mut)]
    pub fn write<W: Write>(self, mut writer: W) -> CatResult<()> {
        self.write_inner(&mut writer)
    }

    fn write_inner(mut self, writer: &mut dyn Write) -> CatResult<()> {
        let settings_ext = self.settings;
        let decode_queue_recv = self.queue_iter.take().ok_or(Error::Aborted)?;

        let (diff_queue, diff_queue_recv) = ordqueue::new(150);
        let (quant_queue, quant_queue_recv) = crossbeam_channel::bounded(150);
        let (remap_queue, remap_queue_recv) = ordqueue::new(150);
        let (write_queue, write_queue_recv) = crossbeam_channel::bounded(150);

        let res = Self::make_resize(decode_queue_recv, diff_queue, &settings_ext);
        if let Err(err) = res {
            panic!("[PRODUCER][RESIZE]: {}", err);
        }

        let res = Self::make_diffs(diff_queue_recv, quant_queue, &settings_ext);
        if let Err(err) = res {
            panic!("[PRODUCER][DIFF]: {}", err);
        }

        let res = Self::quantize_frames(quant_queue_recv, remap_queue, &settings_ext);
        if let Err(err) = res {
            panic!("[PRODUCER][QUANT]: {}", err);
        }

        let res = Self::remap_frames(remap_queue_recv, write_queue);
        if let Err(err) = res {
            panic!("[PRODUCER][REMAP]: {}", err);
        }

        let out = Self::write_frames(write_queue_recv, writer, &settings_ext);
        if let Err(err) = &out {
            panic!("[PRODUCER][WRITE]: {}", err);
        }

        Ok(())
    }

    /// Apply resizing and crate a blurred version for the diff/denoise phase
    fn make_resize(
        inputs: Receiver<InputFrameUnresized>,
        diff_queue: OrdQueue<InputFrame>,
        settings: &SettingsExt,
    ) -> CatResult<()> {
        for frame in inputs {
            // TODO: Remove resize
            // let resized = resized_binary_alpha(frame.frame, settings.s.width, settings.s.height)?;
            let resized = frame.frame;
            let frame_blurred = if settings.extra_effort {
                smart_blur(resized.as_ref())
            } else {
                less_smart_blur(resized.as_ref())
            };
            diff_queue.push(
                frame.frame_index,
                InputFrame {
                    frame: resized,
                    frame_blurred,
                    presentation_timestamp: frame.presentation_timestamp,
                },
            )?;
            // log::info!("pushed frame {} to diff queue", frame.frame_index + 1);
        }
        Ok(())
    }

    /// Find differences between frames, and compute importance maps
    fn make_diffs(
        mut inputs: OrdQueueIter<InputFrame>,
        quant_queue: Sender<DiffMessage>,
        settings: &SettingsExt,
    ) -> CatResult<()> {
        let first_frame = inputs.next().ok_or(Error::NoFrames)?;

        let mut last_frame_duration = if first_frame.presentation_timestamp > 1. / 100. {
            // this is gifski's weird rule that a non-zero first-frame pts
            // shifts the whole anim and is the delay of the last frame
            LastFrameDuration::FixedOffset(first_frame.presentation_timestamp)
        } else {
            LastFrameDuration::FrameRate(0.)
        };

        let mut denoiser = Denoiser::new(
            first_frame.frame.width(),
            first_frame.frame.height(),
            settings.motion_quality,
        );

        let mut next_frame = Some(first_frame);
        let mut ordinal_frame_number = 0;
        let mut last_frame_pts = 0.;
        loop {
            // NB! There are two interleaved loops here:
            //  - one to feed the denoiser
            //  - the other to process denoised frames
            //
            // The denoiser buffers five frames, so these two loops process different frames!
            // But need to be interleaved in one `loop{}` to get frames falling out of denoiser's buffer.

            ////////////////////// Feed denoiser: /////////////////////

            let curr_frame = next_frame.take();
            next_frame = inputs.next();

            if let Some(InputFrame {
                frame,
                frame_blurred,
                presentation_timestamp: raw_pts,
            }) = curr_frame
            {
                ordinal_frame_number += 1;

                let pts = raw_pts - last_frame_duration.shift_every_pts_by();
                if let LastFrameDuration::FrameRate(duration) = &mut last_frame_duration {
                    *duration = pts - last_frame_pts;
                }
                last_frame_pts = pts;

                denoiser
                    .push_frame(
                        frame.as_ref(),
                        frame_blurred.as_ref(),
                        (ordinal_frame_number, pts, last_frame_duration),
                    )
                    .map_err(|_| {
                        Error::WrongSize(format!(
                            "Frame {ordinal_frame_number} has wrong size ({}×{})",
                            frame.width(),
                            frame.height()
                        ))
                    })?;
                if next_frame.is_none() {
                    denoiser.flush();
                }
            }

            ////////////////////// Consume denoised frames /////////////////////

            let (importance_map, image, (ordinal_frame_number, pts, last_frame_duration)) =
                match denoiser.pop() {
                    Denoised::Done => {
                        debug_assert!(next_frame.is_none());
                        break;
                    }
                    Denoised::NotYet => continue,
                    Denoised::Frame {
                        importance_map,
                        frame,
                        meta,
                    } => (importance_map, frame, meta),
                };

            let (importance_map, ..) = importance_map.into_contiguous_buf();

            quant_queue.send(DiffMessage {
                importance_map,
                ordinal_frame_number,
                image,
                pts,
                frame_duration: last_frame_duration.value().max(1. / 100.),
            })?;
            // log::info!("puhed frame {} to quant queue ", ordinal_frame_number)
        }

        Ok(())
    }

    fn quantize_frames(
        inputs: Receiver<DiffMessage>,
        remap_queue: OrdQueue<RemapMessage>,
        settings: &SettingsExt,
    ) -> CatResult<()> {
        let mut inputs = inputs.into_iter().peekable();

        let DiffMessage {
            image: first_frame, ..
        } = inputs.peek().ok_or(Error::NoFrames)?;
        let first_frame_has_transparency = first_frame.pixels().any(|px| px.a < 128);

        let mut prev_frame_keeps = false;
        let mut frame_index = 0;
        let mut importance_map = None;
        while let Some(DiffMessage {
            mut image,
            pts,
            frame_duration,
            ordinal_frame_number,
            importance_map: new_importance_map,
        }) = inputs.next()
        {
            if importance_map.is_none() {
                importance_map = Some(new_importance_map);
            }

            let dispose = if let Some(DiffMessage {
                image: next_image, ..
            }) = inputs.peek()
            {
                // Skip identical frames
                if next_image.as_ref() == image.as_ref() {
                    // this keeps importance_map of the previous frame in the identical-frame series
                    // (important, because subsequent identical frames have all-zero importance_map and would be dropped too)
                    continue;
                }

                // If the next frame becomes transparent, this frame has to clear to bg for it
                if next_image
                    .pixels()
                    .zip(image.pixels())
                    .any(|(next, curr)| next.a < curr.a)
                {
                    gif::DisposalMethod::Background
                } else {
                    gif::DisposalMethod::Keep
                }
            } else if first_frame_has_transparency {
                // Last frame should reset to background to avoid breaking transparent looped anims
                gif::DisposalMethod::Background
            } else {
                // macOS preview gets Background wrong
                gif::DisposalMethod::Keep
            };

            let importance_map = importance_map.take().ok_or(Error::ThreadSend)?; // always set at the beginning
            if !prev_frame_keeps || importance_map.iter().any(|&px| px > 0) {
                if prev_frame_keeps {
                    // if denoiser says the background didn't change, then believe it
                    // (except higher quality settings, which try to improve it every time)
                    let bg_keep_likelihood = u32::from(settings.s.quality.saturating_sub(80) / 4);
                    if settings.s.fast
                        || (settings.s.quality < 100 && (frame_index % 5) >= bg_keep_likelihood)
                    {
                        image
                            .pixels_mut()
                            .zip(&importance_map)
                            .filter(|&(_, &m)| m == 0)
                            .for_each(|(px, _)| *px = RGBA8::new(0, 0, 0, 0));
                    }
                }

                let end_pts = if let Some(&DiffMessage { pts: next_pts, .. }) = inputs.peek() {
                    next_pts
                } else {
                    pts + frame_duration
                };
                debug_assert!(end_pts > 0.);

                let needs_transparency =
                    frame_index > 0 || (frame_index == 0 && first_frame_has_transparency);

                let mut importance_map = importance_map.clone();
                let (liq, remap, liq_image, out_buf) = Self::quantize(
                    image,
                    &importance_map,
                    frame_index == 0,
                    needs_transparency,
                    prev_frame_keeps,
                    settings,
                )?;
                let max_loss = settings.gifsicle_loss();
                for imp in &mut importance_map {
                    // encoding assumes rgba background looks like encoded background, which is not true for lossy
                    *imp = ((256 - u32::from(*imp)) * max_loss / 256).min(255) as u8;
                }

                remap_queue.push(
                    frame_index as usize,
                    RemapMessage {
                        ordinal_frame_number,
                        end_pts,
                        dispose,
                        liq,
                        remap,
                        liq_image,
                        out_buf,
                    },
                )?;
                // log::info!("pushed frame {} to remap queue", frame_index);

                frame_index += 1;
                prev_frame_keeps = dispose == gif::DisposalMethod::Keep;
            }
        }

        Ok(())
    }

    fn remap_frames(
        inputs: OrdQueueIter<RemapMessage>,
        write_queue: Sender<FrameMessage>,
    ) -> CatResult<()> {
        let mut inputs = inputs.into_iter().peekable();
        let first_frame = inputs.peek().ok_or(Error::NoFrames)?;
        let mut screen = gif_dispose::Screen::new(
            first_frame.liq_image.width(),
            first_frame.liq_image.height(),
            RGBA8::new(0, 0, 0, 0),
            None,
        );

        let mut frame_index = 0;
        while let Some(RemapMessage {
            ordinal_frame_number,
            end_pts,
            dispose,
            liq,
            remap,
            liq_image,
            out_buf,
        }) = inputs.next()
        {
            let screen_width = screen.pixels.width() as u16;
            let screen_height = screen.pixels.height() as u16;
            let mut screen_after_dispose = screen.dispose();

            let (mut image8, mut image8_pal) = {
                let bg = if frame_index != 0 {
                    Some(screen_after_dispose.pixels())
                } else {
                    None
                };
                Self::remap(liq, remap, liq_image, bg, out_buf)?
            };

            let transparent_index =
                transparent_index_from_palette(&mut image8_pal, image8.as_mut());

            let next_frame = inputs.peek();
            let (left, top, image8) = if frame_index != 0 && next_frame.is_some() {
                match trim_image(
                    image8,
                    &image8_pal,
                    transparent_index,
                    screen_after_dispose.pixels(),
                ) {
                    Some(trimmed) => trimmed,
                    None => continue, // no pixels need to be changed after dispose
                }
            } else {
                // must keep first and last frame
                (0, 0, image8)
            };

            screen_after_dispose.then_blit(
                Some(&image8_pal),
                dispose,
                left,
                top as _,
                image8.as_ref(),
                transparent_index,
            )?;

            write_queue.send(FrameMessage {
                ordinal_frame_number,
                end_pts,
                screen_width,
                screen_height,
                frame: GIFFrame {
                    left,
                    top,
                    image: image8,
                    pal: image8_pal,
                    transparent_index,
                    dispose,
                },
            })?;

            frame_index += 1;
            // log::info!("pushed frame {} to write queue", ordinal_frame_number);
        }
        Ok(())
    }
}

fn transparent_index_from_palette(
    image8_pal: &mut [RGBA<u8>],
    mut image8: ImgRefMut<u8>,
) -> Option<u8> {
    // Palette may have multiple transparent indices :(
    let mut transparent_index = None;
    for (i, p) in image8_pal.iter_mut().enumerate() {
        if p.a <= 128 {
            p.a = 0;
            let new_index = i as u8;
            if let Some(old_index) = transparent_index {
                image8
                    .pixels_mut()
                    .filter(|px| **px == new_index)
                    .for_each(|px| *px = old_index);
            } else {
                transparent_index = Some(new_index);
            }
        }
    }

    // Check that palette is fine and has no duplicate transparent indices
    debug_assert!(image8_pal.iter().enumerate().all(|(idx, color)| {
        Some(idx as u8) == transparent_index
            || color.a > 128
            || !image8.pixels().any(|px| px == idx as u8)
    }));

    transparent_index
}

fn trim_image(
    mut image8: ImgVec<u8>,
    image8_pal: &[RGBA8],
    transparent_index: Option<u8>,
    mut screen: ImgRef<RGBA8>,
) -> Option<(u16, u16, ImgVec<u8>)> {
    let mut image_trimmed = image8.as_ref();

    let bottom = image_trimmed
        .rows()
        .zip(screen.rows())
        .rev()
        .take_while(|(img_row, screen_row)| {
            img_row
                .iter()
                .copied()
                .zip(screen_row.iter().copied())
                .all(|(px, bg)| {
                    Some(px) == transparent_index || image8_pal.get(px as usize) == Some(&bg)
                })
        })
        .count();

    if bottom > 0 {
        if bottom == image_trimmed.height() {
            return None;
        }
        image_trimmed =
            image_trimmed.sub_image(0, 0, image_trimmed.width(), image_trimmed.height() - bottom);
        screen = screen.sub_image(0, 0, screen.width(), screen.height() - bottom);
    }

    let top = image_trimmed
        .rows()
        .zip(screen.rows())
        .take_while(|(img_row, screen_row)| {
            img_row
                .iter()
                .copied()
                .zip(screen_row.iter().copied())
                .all(|(px, bg)| {
                    Some(px) == transparent_index || image8_pal.get(px as usize) == Some(&bg)
                })
        })
        .count();

    if top > 0 {
        image_trimmed =
            image_trimmed.sub_image(0, top, image_trimmed.width(), image_trimmed.height() - top);
        screen = screen.sub_image(0, top, screen.width(), screen.height() - top);
    }

    let left = (0..image_trimmed.width() - 1)
        .take_while(|&x| {
            (0..image_trimmed.height()).all(|y| {
                let px = image_trimmed[(x, y)];
                Some(px) == transparent_index
                    || image8_pal.get(px as usize) == Some(&screen[(x, y)])
            })
        })
        .count();
    if left > 0 {
        image_trimmed = image_trimmed.sub_image(
            left,
            0,
            image_trimmed.width() - left,
            image_trimmed.height(),
        );
    }

    if image_trimmed.height() != image8.height() || image_trimmed.width() != image8.width() {
        let (buf, width, height) = image_trimmed.to_contiguous_buf();
        image8 = Img::new(buf.into_owned(), width, height);
    }

    Some((left as _, top as _, image8))
}

trait PushInCapacity<T> {
    fn push_in_cap(&mut self, val: T);
}

impl<T> PushInCapacity<T> for Vec<T> {
    #[track_caller]
    #[inline(always)]
    fn push_in_cap(&mut self, val: T) {
        debug_assert!(self.capacity() != self.len());
        if self.capacity() != self.len() {
            self.push(val);
        }
    }
}
