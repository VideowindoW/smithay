//! Utilities for manipulating the data devices
//!
//! The data device is wayland's abstraction to represent both selection (copy/paste) and
//! drag'n'drop actions. This module provides logic to handle this part of the protocol.
//! Selection and drag'n'drop are per-seat notions.
//!
//! This module provides the freestanding [`set_data_device_focus`] function:
//!   This function sets the data device focus for a given seat; you'd typically call it
//!   whenever the keyboard focus changes, to follow it (for example in the focus hook of your keyboards).
//!
//! Using these two functions is enough for your clients to be able to interact with each other using
//! the data devices.
//!
//! The module also provides additional mechanisms allowing your compositor to see and interact with
//! the contents of the data device:
//!
//! - the freestanding function [`set_data_device_selection`]
//!   allows you to set the contents of the selection for your clients
//! - the freestanding function [`start_dnd`] allows you to initiate a drag'n'drop event from the compositor
//!   itself and receive interactions of clients with it via an other dedicated callback.
//!
//! The module defines the role `"dnd_icon"` that is assigned to surfaces used as drag'n'drop icons.
//!
//! ## Initialization
//!
//! To initialize this implementation, create the [`DataDeviceState`], store it inside your `State` struct
//! and implement the [`DataDeviceHandler`], as shown in this example:
//!
//! ```
//! # extern crate wayland_server;
//! # #[macro_use] extern crate smithay;
//! use smithay::delegate_data_device;
//! use smithay::wayland::data_device::{ClientDndGrabHandler, DataDeviceState, DataDeviceHandler, ServerDndGrabHandler};
//!
//! # struct State { data_device_state: DataDeviceState }
//! # let mut display = wayland_server::Display::<State>::new().unwrap();
//! // Create the data_device state
//! let data_device_state = DataDeviceState::new::<State, _>(
//!     &display.handle(),
//!     None // We don't add a logger in this example
//! );
//!
//! // insert the DataDeviceState into your state
//! // ..
//!
//! // implement the necessary traits
//! impl ClientDndGrabHandler for State {}
//! impl ServerDndGrabHandler for State {}
//! impl DataDeviceHandler for State {
//!     fn data_device_state(&self) -> &DataDeviceState { &self.data_device_state }
//!     // ... override default implementations here to customize handling ...
//! }
//! delegate_data_device!(State);
//!
//! // You're now ready to go!
//! ```

use std::{cell::RefCell, os::unix::prelude::RawFd};

use wayland_server::{
    backend::GlobalId,
    protocol::{
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_source::WlDataSource,
        wl_surface::WlSurface,
    },
    Client, DisplayHandle, GlobalDispatch,
};

use super::Serial;
use crate::input::{
    pointer::{Focus, GrabStartData as PointerGrabStartData},
    Seat, SeatHandler,
};

mod device;
mod dnd_grab;
mod seat_data;
mod server_dnd_grab;
mod source;

pub use device::{DataDeviceUserData, DND_ICON_ROLE};
pub use source::{with_source_metadata, DataSourceUserData, SourceMetadata};

use seat_data::{SeatData, Selection};

/// Events that are generated by interactions of the clients with the data device
#[allow(unused_variables)]
pub trait DataDeviceHandler: Sized + ClientDndGrabHandler + ServerDndGrabHandler {
    /// [DataDeviceState] getter
    fn data_device_state(&self) -> &DataDeviceState;

    /// Action chooser for DnD negociation
    fn action_choice(&mut self, available: DndAction, preferred: DndAction) -> DndAction {
        default_action_chooser(available, preferred)
    }

    /// A client has set the selection
    fn new_selection(&mut self, source: Option<WlDataSource>) {}

    /// A client requested to read the server-set selection
    ///
    /// * `mime_type` - the requested mime type
    /// * `fd` - the fd to write into
    fn send_selection(&mut self, mime_type: String, fd: RawFd) {}
}

/// Events that are generated during client initiated drag'n'drop
#[allow(unused_variables)]
pub trait ClientDndGrabHandler: SeatHandler + Sized {
    /// A client started a drag'n'drop as response to a user pointer action
    ///
    /// * `source` - The data source provided by the client.
    ///              If it is `None`, this means the DnD is restricted to surfaces of the
    ///              same client and the client will manage data transfer by itself.
    /// * `icon` - The icon the client requested to be used to be associated with the cursor icon
    ///            during the drag'n'drop.
    /// * `seat` - The seat on which the DnD operation was started
    fn started(&mut self, source: Option<WlDataSource>, icon: Option<WlSurface>, seat: Seat<Self>) {}

    /// The drag'n'drop action was finished by the user releasing the buttons
    ///
    /// At this point, any pointer icon should be removed.
    ///
    /// Note that this event will only be generated for client-initiated drag'n'drop session.
    ///
    /// * `seat` - The seat on which the DnD action was finished.
    fn dropped(&mut self, seat: Seat<Self>) {}
}

/// Event generated by the interactions of clients with a server initiated drag'n'drop
#[allow(unused_variables)]
pub trait ServerDndGrabHandler {
    /// The client chose an action
    fn action(&mut self, action: DndAction) {}

    /// The DnD resource was dropped by the user
    ///
    /// After that, the client can still interact with your resource
    fn dropped(&mut self) {}

    /// The Dnd was cancelled
    ///
    /// The client can no longer interact
    fn cancelled(&mut self) {}

    /// The client requested for data to be sent
    ///
    /// * `mime_type` - The requested mime type
    /// * `fd` - The FD to write into
    fn send(&mut self, mime_type: String, fd: RawFd) {}

    /// The client has finished interacting with the resource
    ///
    /// This can only happen after the resource was dropped.
    fn finished(&mut self) {}
}

/// State of data device
#[derive(Debug)]
pub struct DataDeviceState {
    log: slog::Logger,
    manager_global: GlobalId,
}

impl DataDeviceState {
    /// Regiseter new [WlDataDeviceManager] global
    pub fn new<D, L>(display: &DisplayHandle, logger: L) -> Self
    where
        L: Into<Option<::slog::Logger>>,
        D: GlobalDispatch<WlDataDeviceManager, ()> + 'static,
        D: DataDeviceHandler,
    {
        let log = crate::slog_or_fallback(logger).new(slog::o!("smithay_module" => "data_device_mgr"));

        let manager_global = display.create_global::<D, WlDataDeviceManager, _>(3, ());

        Self { log, manager_global }
    }

    /// [WlDataDeviceManager] GlobalId getter
    pub fn global(&self) -> GlobalId {
        self.manager_global.clone()
    }
}

/// A simple action chooser for DnD negociation
///
/// If the preferred action is available, it'll pick it. Otherwise, it'll pick the first
/// available in the following order: Ask, Copy, Move.
pub fn default_action_chooser(available: DndAction, preferred: DndAction) -> DndAction {
    // if the preferred action is valid (a single action) and in the available actions, use it
    // otherwise, follow a fallback stategy
    if [DndAction::Move, DndAction::Copy, DndAction::Ask].contains(&preferred)
        && available.contains(preferred)
    {
        preferred
    } else if available.contains(DndAction::Ask) {
        DndAction::Ask
    } else if available.contains(DndAction::Copy) {
        DndAction::Copy
    } else if available.contains(DndAction::Move) {
        DndAction::Move
    } else {
        DndAction::empty()
    }
}

/// Set the data device focus to a certain client for a given seat
pub fn set_data_device_focus<D>(dh: &DisplayHandle, seat: &Seat<D>, client: Option<Client>)
where
    D: SeatHandler + DataDeviceHandler + 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::new()));
    let seat_data = seat.user_data().get::<RefCell<SeatData>>().unwrap();
    seat_data.borrow_mut().set_focus::<D>(dh, client);
}

/// Set a compositor-provided selection for this seat
///
/// You need to provide the available mime types for this selection.
///
/// Whenever a client requests to read the selection, your callback will
/// receive a [`DataDeviceHandler::send_selection`] event.
pub fn set_data_device_selection<D>(dh: &DisplayHandle, seat: &Seat<D>, mime_types: Vec<String>)
where
    D: SeatHandler + DataDeviceHandler + 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::new()));
    let seat_data = seat.user_data().get::<RefCell<SeatData>>().unwrap();
    seat_data.borrow_mut().set_selection::<D>(
        dh,
        Selection::Compositor(SourceMetadata {
            mime_types,
            dnd_action: DndAction::empty(),
        }),
    );
}

/// Start a drag'n'drop from a resource controlled by the compositor
///
/// You'll receive events generated by the interaction of clients with your
/// drag'n'drop in the provided callback. See [`ServerDndGrabHandler`] for details about
/// which events can be generated and what response is expected from you to them.
pub fn start_dnd<D, C>(
    dh: &DisplayHandle,
    seat: &Seat<D>,
    data: &mut D,
    serial: Serial,
    start_data: PointerGrabStartData<D>,
    metadata: SourceMetadata,
) where
    D: SeatHandler + DataDeviceHandler + 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::new()));
    if let Some(pointer) = seat.get_pointer() {
        pointer.set_grab(
            data,
            server_dnd_grab::ServerDnDGrab::new(dh, start_data, metadata, seat.clone()),
            serial,
            Focus::Keep,
        );
    }
}

mod handlers {
    use std::cell::RefCell;

    use slog::error;
    use wayland_server::{
        protocol::{
            wl_data_device::WlDataDevice,
            wl_data_device_manager::{self, WlDataDeviceManager},
            wl_data_source::WlDataSource,
        },
        Dispatch, DisplayHandle, GlobalDispatch,
    };

    use crate::input::Seat;

    use super::{device::DataDeviceUserData, seat_data::SeatData, source::DataSourceUserData};
    use super::{DataDeviceHandler, DataDeviceState};

    impl<D> GlobalDispatch<WlDataDeviceManager, (), D> for DataDeviceState
    where
        D: GlobalDispatch<WlDataDeviceManager, ()>,
        D: Dispatch<WlDataDeviceManager, ()>,
        D: Dispatch<WlDataSource, DataSourceUserData>,
        D: Dispatch<WlDataDevice, DataDeviceUserData>,
        D: DataDeviceHandler,
        D: 'static,
    {
        fn bind(
            _state: &mut D,
            _handle: &DisplayHandle,
            _client: &wayland_server::Client,
            resource: wayland_server::New<WlDataDeviceManager>,
            _global_data: &(),
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl<D> Dispatch<WlDataDeviceManager, (), D> for DataDeviceState
    where
        D: Dispatch<WlDataDeviceManager, ()>,
        D: Dispatch<WlDataSource, DataSourceUserData>,
        D: Dispatch<WlDataDevice, DataDeviceUserData>,
        D: DataDeviceHandler,
        D: 'static,
    {
        fn request(
            state: &mut D,
            _client: &wayland_server::Client,
            _resource: &WlDataDeviceManager,
            request: wl_data_device_manager::Request,
            _data: &(),
            _dhandle: &DisplayHandle,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            let data_device_state = state.data_device_state();

            match request {
                wl_data_device_manager::Request::CreateDataSource { id } => {
                    data_init.init(id, DataSourceUserData::new());
                }
                wl_data_device_manager::Request::GetDataDevice { id, seat: wl_seat } => {
                    match Seat::<D>::from_resource(&wl_seat) {
                        Some(seat) => {
                            seat.user_data()
                                .insert_if_missing(|| RefCell::new(SeatData::new()));

                            let data_device = data_init.init(id, DataDeviceUserData { wl_seat });

                            let seat_data = seat.user_data().get::<RefCell<SeatData>>().unwrap();
                            seat_data.borrow_mut().add_device(data_device);
                        }
                        None => {
                            error!(&data_device_state.log, "Unmanaged seat given to a data device.");
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

#[allow(missing_docs)] // TODO
#[macro_export]
macro_rules! delegate_data_device {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        $crate::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_server::protocol::wl_data_device_manager::WlDataDeviceManager: ()
        ] => $crate::wayland::data_device::DataDeviceState);

        $crate::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_server::protocol::wl_data_device_manager::WlDataDeviceManager: ()
        ] => $crate::wayland::data_device::DataDeviceState);
        $crate::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_server::protocol::wl_data_device::WlDataDevice: $crate::wayland::data_device::DataDeviceUserData
        ] => $crate::wayland::data_device::DataDeviceState);
        $crate::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_server::protocol::wl_data_source::WlDataSource: $crate::wayland::data_device::DataSourceUserData
        ] => $crate::wayland::data_device::DataDeviceState);
    };
}
