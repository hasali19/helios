use std::{
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use eyre::eyre;
use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, ButtonState, Event, InputBackend, InputEvent, KeyboardKeyEvent,
            PointerButtonEvent,
        },
        renderer::{
            damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer, utils::on_commit_buffer_handler,
        },
        winit::{self, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    },
    delegate_dispatch2,
    desktop::{Space, Window, WindowSurfaceType},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::FilterResult,
        pointer::{ButtonEvent, CursorImageStatus, MotionEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode as PollMode, PostAction, generic::Generic,
        },
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
    },
};
use tracing_subscriber::EnvFilter;

struct Helios {
    start_time: Instant,
    socket_name: OsString,
    display_handle: DisplayHandle,

    space: Space<Window>,
    loop_signal: LoopSignal,

    seat: Seat<Self>,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
}

impl Helios {
    fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
    ) -> eyre::Result<Self> {
        let start_time = Instant::now();
        let dh = display.handle();

        // Initialize protocols needed for displaying windows
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        seat.add_keyboard(Default::default(), 200, 25)?;

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        // Setup a wayland socket that will be used to accept clients
        let socket_name = Self::listen(display, event_loop)?;

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        Ok(Self {
            start_time,
            socket_name,
            display_handle: dh,

            space,
            loop_signal,

            seat,

            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
        })
    }

    /// Opens a wayland socket for clients and hooks the display into the event loop.
    fn listen(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> eyre::Result<OsString> {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let socket = ListeningSocketSource::new_auto()?;

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = socket.socket_name().to_os_string();

        let handle = event_loop.handle();

        handle.insert_source(socket, |stream, _, state| {
            // Inside the callback, you should insert the client into the display.
            //
            // You may also associate some data with the client when inserting the client.
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to insert client: {err}");
            }
        })?;

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        handle.insert_source(
            Generic::new(display, Interest::READ, PollMode::Level),
            |_, display, state| {
                // Safety: the display is never dropped while it is registered.
                unsafe { display.get_mut().dispatch_clients(state)? };
                Ok(PostAction::Continue)
            },
        )?;

        Ok(socket_name)
    }

    /// Finds the topmost surface under the given point and returns it along with its location in the space
    fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Get the topmost window at this point and its location in space
        let (window, location) = self.space.element_under(pos)?;
        // Get the topmost surface within the window at this point, and its offset within the window
        let (surface, offset) =
            window.surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)?;
        Some((surface, (offset + location).to_f64()))
    }

    fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time();

                self.seat.get_keyboard().unwrap().input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(output_size) = self
                    .space
                    .outputs()
                    .next()
                    .and_then(|o| self.space.output_geometry(o))
                else {
                    return;
                };

                let location =
                    event.position_transformed(output_size.size) + output_size.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();
                let surface = self.surface_under(location);

                let pointer = self.seat.get_pointer().unwrap();

                pointer.motion(
                    self,
                    surface,
                    &MotionEvent {
                        location,
                        serial,
                        time: event.time(),
                    },
                );

                pointer.frame(self);
            }
            InputEvent::PointerButton { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();
                let button_state = event.state();

                if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
                    // Find the window under the pointer
                    if let Some(window) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, _)| w.clone())
                    {
                        // Move the window to the top of the stack and activate it
                        self.space.raise_element(&window, true);

                        // Set keyboard focus to the window's toplevel surface
                        keyboard.set_focus(
                            self,
                            Some(window.toplevel().unwrap().wl_surface().clone()),
                            serial,
                        );

                        for window in self.space.elements() {
                            window.toplevel().unwrap().send_pending_configure();
                        }
                    } else {
                        // Pointer is in empty space so deactivate all windows
                        for window in self.space.elements() {
                            window.set_activated(false);
                            window.toplevel().unwrap().send_pending_configure();
                        }

                        // Clear keyboard focus
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time(),
                    },
                );

                pointer.frame(self);
            }
            _ => {}
        }
    }
}

// We can use this to associate data with each client that connects to the compositor.
#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl CompositorHandler for Helios {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if is_sync_subsurface(surface) {
            return;
        }

        // Find the root surface
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        // Find the window that contains this surface and commit it
        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        {
            window.on_commit();

            let initial_configure_sent = with_states(&root, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });

            if !initial_configure_sent {
                window.toplevel().unwrap().send_configure();
            }
        }
    }
}

impl BufferHandler for Helios {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Helios {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for Helios {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window, (0, 0), false);
    }

    // TODO
    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl SeatHandler for Helios {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl OutputHandler for Helios {}

impl PointerConstraintsHandler for Helios {}

delegate_dispatch2!(Helios);

/// Opens a host window (nested compositor) and renders the space into it.
fn init_winit(
    event_loop: &mut EventLoop<'static, Helios>,
    mut backend: WinitGraphicsBackend<GlesRenderer>,
    winit_loop: WinitEventLoop,
    state: &mut Helios,
) -> eyre::Result<()> {
    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Helios".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );

    output.create_global::<Helios>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit_loop, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);

                {
                    let (renderer, mut framebuffer) = backend.bind().unwrap();
                    smithay::desktop::space::render_output::<
                        _,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                        _,
                        _,
                    >(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0,
                        0,
                        [&state.space],
                        &[],
                        &mut damage_tracker,
                        [0.1, 0.1, 0.1, 1.0],
                    )
                    .unwrap();
                }
                backend.submit(Some(&[damage])).unwrap();

                for window in state.space.elements() {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }

                state.space.refresh();
                let _ = state.display_handle.flush_clients();

                // Ask for redraw to schedule new frame.
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => {}
        })
        .map_err(|e| eyre!("{e}"))?;

    Ok(())
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    if let Ok(env_filter) = EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<Helios> = EventLoop::try_new()?;
    let display: Display<Helios> = Display::new()?;
    let mut state = Helios::new(&mut event_loop, display)?;

    let (winit_backend, winit_loop) = winit::init::<GlesRenderer>().map_err(|e| eyre!("{e}"))?;

    init_winit(&mut event_loop, winit_backend, winit_loop, &mut state)?;

    tracing::info!("listening on {:?}", state.socket_name);

    // Optionally launch a client: `helios -c <command>`.
    let mut args = std::env::args().skip(1);
    if let (Some("-c" | "--command"), Some(command)) = (args.next().as_deref(), args.next()) {
        std::process::Command::new(command)
            .env("WAYLAND_DISPLAY", &state.socket_name)
            .spawn()?;
    }

    event_loop.run(None, &mut state, |_| {})?;

    Ok(())
}
