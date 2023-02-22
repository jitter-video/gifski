use crate::error::CatResult;
use crate::GIFFrame;
use crate::Settings;
use crate::SettingsExt;
use rgb::ComponentBytes;
use std::io::Write;

pub(crate) struct RustEncoder<W: Write> {
    writer: Option<W>,
    gif_enc: Option<gif::Encoder<W>>,
}

impl<W: Write> RustEncoder<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: Some(writer),
            gif_enc: None,
        }
    }
}

impl<W: Write> RustEncoder<W> {
    pub fn compress_frame(f: GIFFrame, settings: &SettingsExt) -> CatResult<gif::Frame<'static>> {
        let GIFFrame {
            left,
            top,
            pal,
            image,
            dispose,
            transparent_index,
        } = f;

        let (buffer, width, height) = image.into_contiguous_buf();

        let mut pal_rgb = Vec::with_capacity(3 * pal.len());
        for p in &pal {
            pal_rgb.extend_from_slice([p.rgb()].as_bytes());
        }

        let mut frame = gif::Frame {
            delay: 1, // TBD
            dispose,
            transparent: transparent_index,
            needs_user_input: false,
            top,
            left,
            width: width as u16,
            height: height as u16,
            interlaced: false,
            palette: Some(pal_rgb),
            buffer: buffer.into(),
        };

        #[allow(unused)]
        let loss = settings.gifsicle_loss();

        frame.make_lzw_pre_encoded();
        Ok(frame)
    }

    pub fn write_frame(
        &mut self,
        mut frame: gif::Frame<'static>,
        delay: u16,
        screen_width: u16,
        screen_height: u16,
        settings: &Settings,
    ) -> CatResult<()> {
        frame.delay = delay; // the delay wasn't known

        let writer = &mut self.writer;
        let enc = match self.gif_enc {
            None => {
                let w = writer.take().ok_or(crate::Error::ThreadSend)?;
                let mut enc = gif::Encoder::new(w, screen_width, screen_height, &[])?;
                enc.write_extension(gif::ExtensionData::Repetitions(settings.repeat))?;
                enc.write_raw_extension(gif::Extension::Comment.into(), &[b"gif.ski"])?;
                self.gif_enc.get_or_insert(enc)
            }
            Some(ref mut enc) => enc,
        };

        enc.write_lzw_pre_encoded_frame(&frame)?;
        Ok(())
    }
}
