//! `app-host`: one app's process. It runs the app's JavaScript bundle in QuickJS
//! ([`runtime`]) with the bus, and installs apps ([`install`]).

pub mod install;
pub mod runtime;
