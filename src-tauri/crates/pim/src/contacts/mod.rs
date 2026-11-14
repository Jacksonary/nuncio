//! Contacts: providers (Google People, Microsoft Graph, CardDAV), sync engine,
//! command facade and pending operations.

pub mod dav;
pub mod google;
pub mod graph;
pub mod map;
pub mod ops;
pub mod provider;
pub mod service;

pub use dav::CardDavProvider;
pub use google::{GoogleContactsProvider, PeopleClient};
pub use graph::{GraphContactsClient, GraphContactsProvider};
pub use ops::{ContactOp, ContactSnapshot};
pub use provider::{
    ContactCursor, ContactDelta, ContactProvider, RemoteAddressBook, RemoteContact,
    RemoteContactRef, RemoteGroup, RemoteWriteResult,
};
pub use service::ContactsService;
