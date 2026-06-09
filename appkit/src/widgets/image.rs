use crate::constraints::{BoxConstraints, ImageScale, Rect, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::node::ImageData;
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};
use slopos_abi::damage::DamageRect;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};

pub struct ImageWidget {
    id: WidgetId,
    rect: Rect,
    image: ImageData,
    scale: ImageScale,
    sampling: ImageSampling,
}

impl ImageWidget {
    pub fn new(image: ImageData, scale: ImageScale, sampling: ImageSampling) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            image,
            scale,
            sampling,
        }
    }
}

impl Widget for ImageWidget {
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let size = match self.scale {
            ImageScale::None => Size::new(self.image.width as i32, self.image.height as i32),
            ImageScale::Fit => {
                let sw = self.image.width as i32;
                let sh = self.image.height as i32;
                if sw == 0 || sh == 0 {
                    return constraints.constrain(Size::ZERO);
                }
                let max_w = constraints.max_width.min(i32::MAX / 2);
                let max_h = constraints.max_height.min(i32::MAX / 2);
                // Scale to fit preserving aspect ratio.
                let scale_w = max_w as f64 / sw as f64;
                let scale_h = max_h as f64 / sh as f64;
                let scale = scale_w.min(scale_h).min(1.0);
                Size::new((sw as f64 * scale) as i32, (sh as f64 * scale) as i32)
            }
            ImageScale::Cover => constraints.max_size(),
            ImageScale::Fill => constraints.max_size(),
        };
        constraints.constrain(size)
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let Some(bitmap) = BitmapRef::new(
            self.image.width,
            self.image.height,
            self.image.pixels.as_ref(),
        ) else {
            return;
        };
        let fit = match self.scale {
            ImageScale::None => ImageFit::Actual,
            ImageScale::Fit => ImageFit::Contain,
            ImageScale::Cover => ImageFit::Cover,
            ImageScale::Fill => ImageFit::Stretch,
        };
        let clip = DamageRect {
            x0: ctx.clip.x,
            y0: ctx.clip.y,
            x1: ctx.clip.x + ctx.clip.width - 1,
            y1: ctx.clip.y + ctx.clip.height - 1,
        };
        slopos_gfx::image::draw_image_clipped(
            ctx.buffer,
            bitmap,
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            fit,
            self.sampling,
            &clip,
        );
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::None
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }

    fn id(&self) -> WidgetId {
        self.id
    }

    fn layout_rect(&self) -> Rect {
        self.rect
    }
}
