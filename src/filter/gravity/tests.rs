//! Tests des Gravity-Updaters: URL- und Header-Parsing, Streaming, Cache und beide
//! Update-Wege.
//!
//! Jede Datei im Verzeichnis ist ein eigenes Untermodul mit eigenen Importen; geteilt
//! werden nur die Helfer aus [`helpers`].

mod cache;
mod helpers;
mod http;
mod update;
