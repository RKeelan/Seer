//! The viewer's pipe server: accepts concurrent clients, reads one request
//! from each, hands it to the UI thread and writes back the reply.
