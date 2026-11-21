//! Text through DirectWrite.
//!
//! [`Text`] creates the DirectWrite factory on first use, so a frame with no
//! text never loads it, and keeps one text format per size, weight and
//! alignment.

use std::collections::HashMap;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, ID2D1DeviceContext, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory,
    IDWriteFactory, IDWriteTextFormat,
};
use windows::core::w;

use crate::error::Result;
use crate::model::geometry::Rect;
use crate::render::scene::Align;

/// The system UI font.
const FONT: windows::core::PCWSTR = w!("Segoe UI");

/// DirectWrite state for drawing text.
#[derive(Default)]
pub struct Text {
    factory: Option<IDWriteFactory>,
    formats: HashMap<FormatKey, IDWriteTextFormat>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FormatKey {
    size_centipixels: u32,
    align: Align,
    bold: bool,
}

/// One line of text to draw.
pub struct Line<'a> {
    /// The text.
    pub text: &'a str,
    /// The rectangle it is centred in vertically and aligned in horizontally.
    pub rect: Rect,
    /// The font size in pixels.
    pub size: f64,
    /// The horizontal alignment.
    pub align: Align,
    /// Whether the text is bold.
    pub bold: bool,
}

impl Text {
    /// Draws `line` with `brush`, on one line, trimmed with an ellipsis if
    /// it does not fit.
    pub fn draw(
        &mut self,
        context: &ID2D1DeviceContext,
        line: &Line,
        brush: &ID2D1SolidColorBrush,
    ) -> Result<()> {
        let format = self.format(line.size, line.align, line.bold)?;
        let text: Vec<u16> = line.text.encode_utf16().collect();
        let rect = D2D_RECT_F {
            left: line.rect.x as f32,
            top: line.rect.y as f32,
            right: line.rect.right() as f32,
            bottom: line.rect.bottom() as f32,
        };
        // SAFETY: the text, format, rectangle and brush outlive the call.
        unsafe {
            context.DrawText(
                &text,
                &format,
                &rect,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            )
        };
        Ok(())
    }

    fn format(&mut self, size: f64, align: Align, bold: bool) -> Result<IDWriteTextFormat> {
        let key = FormatKey {
            size_centipixels: (size.max(1.0) * 100.0).round() as u32,
            align,
            bold,
        };
        if let Some(format) = self.formats.get(&key) {
            return Ok(format.clone());
        }
        let factory = match &self.factory {
            Some(factory) => factory.clone(),
            None => {
                // SAFETY: DWriteCreateFactory takes a plain value.
                let factory: IDWriteFactory =
                    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
                self.factory = Some(factory.clone());
                factory
            }
        };
        let weight = if bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        };
        // SAFETY: the strings are static; no font collection means the
        // system's.
        let format = unsafe {
            factory.CreateTextFormat(
                FONT,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                key.size_centipixels as f32 / 100.0,
                w!(""),
            )
        }?;
        // SAFETY: the format is live; these take plain values.
        unsafe {
            format.SetTextAlignment(alignment(align))?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        }
        // SAFETY: the format is live.
        let ellipsis = unsafe { factory.CreateEllipsisTrimmingSign(&format) }?;
        let trimming = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            ..Default::default()
        };
        // SAFETY: the trimming options and sign are live.
        unsafe { format.SetTrimming(&trimming, &ellipsis) }?;
        self.formats.insert(key, format.clone());
        Ok(format)
    }
}

fn alignment(align: Align) -> DWRITE_TEXT_ALIGNMENT {
    match align {
        Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
        Align::Centre => DWRITE_TEXT_ALIGNMENT_CENTER,
        Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
    }
}
