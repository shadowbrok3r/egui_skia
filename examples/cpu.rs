#[cfg(not(feature = "winit"))]
fn main() {
    println!("This example requires the winit feature to be enabled");
}

#[cfg(feature = "winit")]
fn main() {
    example::main();
}

#[cfg(feature = "winit")]
mod example {
    use std::num::NonZeroU32;
    use std::rc::Rc;

    use egui_skia::EguiSkiaWinit;
    use egui_winit::winit::application::ApplicationHandler;
    use egui_winit::winit::dpi::LogicalSize;
    use egui_winit::winit::event::WindowEvent;
    use egui_winit::winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use egui_winit::winit::window::{Window, WindowId};
    use skia_safe::{surfaces, AlphaType, Color, ColorType, ImageInfo, Surface};

    struct App {
        window: Option<Rc<Window>>,
        softbuffer_surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        skia_surface: Option<Surface>,
        egui_skia: Option<EguiSkiaWinit>,
    }

    impl App {
        fn new() -> Self {
            Self {
                window: None,
                softbuffer_surface: None,
                skia_surface: None,
                egui_skia: None,
            }
        }

        fn recreate_skia_surface(&mut self, width: i32, height: i32) {
            self.skia_surface =
                surfaces::raster_n32_premul((width.max(1), height.max(1)));
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let attrs = Window::default_attributes()
                .with_title("egui_skia cpu example")
                .with_inner_size(LogicalSize::new(1024.0, 768.0));
            let window = Rc::new(event_loop.create_window(attrs).unwrap());

            let context = softbuffer::Context::new(window.clone()).unwrap();
            let softbuffer_surface =
                softbuffer::Surface::new(&context, window.clone()).unwrap();

            let egui_skia =
                EguiSkiaWinit::new(window.as_ref(), Some(window.scale_factor() as f32));

            let size = window.inner_size();
            self.recreate_skia_surface(size.width as i32, size.height as i32);

            self.softbuffer_surface = Some(softbuffer_surface);
            self.egui_skia = Some(egui_skia);
            self.window = Some(window);
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _window_id: WindowId,
            event: WindowEvent,
        ) {
            let Some(window) = self.window.clone() else {
                return;
            };

            if let Some(egui_skia) = self.egui_skia.as_mut() {
                let response = egui_skia.on_window_event(&window, &event);
                if response.repaint {
                    window.request_redraw();
                }
            }

            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::Resized(size) => {
                    self.recreate_skia_surface(size.width as i32, size.height as i32);
                    window.request_redraw();
                }
                WindowEvent::RedrawRequested => {
                    let (Some(skia_surface), Some(egui_skia), Some(softbuffer_surface)) = (
                        self.skia_surface.as_mut(),
                        self.egui_skia.as_mut(),
                        self.softbuffer_surface.as_mut(),
                    ) else {
                        return;
                    };
                    let width = skia_surface.width();
                    let height = skia_surface.height();

                    let canvas = skia_surface.canvas();
                    canvas.clear(Color::from_argb(255, 30, 30, 30));

                    let repaint_after = egui_skia.run(&window, |ctx| {
                        egui::Window::new("egui_skia").show(ctx, |ui| {
                            ui.label("Rendered with skia-safe on a CPU raster surface.");
                            if ui.button("Click me").clicked() {
                                println!("clicked");
                            }
                        });
                    });

                    egui_skia.paint(canvas);

                    present(skia_surface, softbuffer_surface, width, height);

                    let control_flow = if repaint_after.is_zero() {
                        window.request_redraw();
                        ControlFlow::Poll
                    } else if let Some(instant) =
                        std::time::Instant::now().checked_add(repaint_after)
                    {
                        ControlFlow::WaitUntil(instant)
                    } else {
                        ControlFlow::Wait
                    };
                    event_loop.set_control_flow(control_flow);
                }
                _ => {}
            }
        }
    }

    fn present(
        skia_surface: &mut Surface,
        softbuffer_surface: &mut softbuffer::Surface<Rc<Window>, Rc<Window>>,
        width: i32,
        height: i32,
    ) {
        let (Some(w), Some(h)) = (
            NonZeroU32::new(width as u32),
            NonZeroU32::new(height as u32),
        ) else {
            return;
        };
        softbuffer_surface.resize(w, h).unwrap();

        // Read into explicit RGBA8888 so the byte order is platform-independent.
        let info = ImageInfo::new(
            (width, height),
            ColorType::RGBA8888,
            AlphaType::Premul,
            None,
        );
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        let ok = skia_surface.read_pixels(&info, &mut rgba, (width * 4) as usize, (0, 0));
        if !ok {
            return;
        }

        let mut buffer = softbuffer_surface.buffer_mut().unwrap();
        for (dst, px) in buffer.iter_mut().zip(rgba.chunks_exact(4)) {
            // softbuffer wants 0x00RRGGBB.
            *dst = ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32);
        }
        buffer.present().unwrap();
    }

    pub fn main() {
        #[cfg(not(feature = "cpu_fix"))]
        eprintln!("Warning! Feature cpu_fix should be enabled when using raster surfaces. See https://github.com/lucasmerlin/egui_skia/issues/1");

        let event_loop = EventLoop::new().unwrap();
        event_loop.set_control_flow(ControlFlow::Wait);
        let mut app = App::new();
        event_loop.run_app(&mut app).unwrap();
    }
}
