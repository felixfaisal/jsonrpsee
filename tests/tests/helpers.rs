// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![cfg(test)]

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use jsonrpsee::core::error::{Error, SubscriptionClosed};
use jsonrpsee::core::server::host_filtering::AllowHosts;
use jsonrpsee::server::middleware::proxy_get_request::ProxyGetRequestLayer;
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use jsonrpsee::types::error::{ErrorObject, SUBSCRIPTION_CLOSED_WITH_ERROR};
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::{PendingSubscriptionSink, RpcModule};
use serde::Serialize;
use tokio::time::interval;
use tokio_stream::wrappers::IntervalStream;
use tower_http::cors::CorsLayer;

#[allow(dead_code)]
pub async fn server_with_subscription_and_handle() -> (SocketAddr, ServerHandle) {
	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();

	let mut module = RpcModule::new(());
	module.register_method("say_hello", |_, _| Ok("hello")).unwrap();

	module
		.register_subscription("subscribe_hello", "subscribe_hello", "unsubscribe_hello", |_, pending, _| {
			let interval = interval(Duration::from_millis(50));
			let stream = IntervalStream::new(interval).map(move |_| &"hello from subscription");

			tokio::spawn(async move {
				pipe_from_stream(stream, pending).await;
			});
			Ok(())
		})
		.unwrap();

	module
		.register_subscription("subscribe_foo", "subscribe_foo", "unsubscribe_foo", |_, pending, _| {
			let interval = interval(Duration::from_millis(100));
			let stream = IntervalStream::new(interval).map(move |_| 1337_usize);

			tokio::spawn(async move {
				pipe_from_stream(stream, pending).await;
			});
			Ok(())
		})
		.unwrap();

	module
		.register_subscription("subscribe_add_one", "subscribe_add_one", "unsubscribe_add_one", |params, pending, _| {
			let params = params.into_owned();
			tokio::spawn(async move {
				let count = match params.one::<usize>().map(|c| c.wrapping_add(1)) {
					Ok(count) => count,
					Err(e) => {
						let _ = pending.reject(ErrorObjectOwned::from(e)).await;
						return;
					}
				};

				let wrapping_counter = futures::stream::iter((count..).cycle());
				let interval = interval(Duration::from_millis(100));
				let stream = IntervalStream::new(interval).zip(wrapping_counter).map(move |(_, c)| c);

				pipe_from_stream(stream, pending).await;
			});
			Ok(())
		})
		.unwrap();

	module
		.register_subscription("subscribe_noop", "subscribe_noop", "unsubscribe_noop", |_, pending, _| {
			tokio::spawn(async move {
				let sink = pending.accept().await.unwrap();
				tokio::time::sleep(Duration::from_secs(1)).await;
				let err = ErrorObject::owned(
					SUBSCRIPTION_CLOSED_WITH_ERROR,
					"Server closed the stream because it was lazy",
					None::<()>,
				);
				sink.close(err).await;
			});
			Ok(())
		})
		.unwrap();

	module
		.register_subscription("subscribe_5_ints", "n", "unsubscribe_5_ints", |_, pending, _| {
			tokio::spawn(async move {
				let interval = interval(Duration::from_millis(50));
				let stream = IntervalStream::new(interval).zip(futures::stream::iter(1..=5)).map(|(_, c)| c);
				pipe_from_stream(stream, pending).await;
			});
			Ok(())
		})
		.unwrap();

	let addr = server.local_addr().unwrap();
	let server_handle = server.start(module).unwrap();

	(addr, server_handle)
}

#[allow(dead_code)]
pub async fn server_with_subscription() -> SocketAddr {
	let (addr, handle) = server_with_subscription_and_handle().await;

	tokio::spawn(handle.stopped());

	addr
}

#[allow(dead_code)]
pub async fn server() -> SocketAddr {
	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let mut module = RpcModule::new(());
	module.register_method("say_hello", |_, _| Ok("hello")).unwrap();

	module
		.register_async_method("slow_hello", |_, _| async {
			tokio::time::sleep(std::time::Duration::from_secs(1)).await;
			Ok("hello")
		})
		.unwrap();

	module.register_async_method("err", |_, _| async { Err::<(), _>(Error::Custom("err".to_string())) }).unwrap();

	let addr = server.local_addr().unwrap();

	let server_handle = server.start(module).unwrap();

	tokio::spawn(server_handle.stopped());

	addr
}

/// Yields one item then sleeps for an hour.
#[allow(dead_code)]
pub async fn server_with_sleeping_subscription(tx: futures::channel::mpsc::Sender<()>) -> SocketAddr {
	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let addr = server.local_addr().unwrap();

	let mut module = RpcModule::new(tx);

	module
		.register_subscription("subscribe_sleep", "n", "unsubscribe_sleep", |_, pending, mut tx| {
			tokio::spawn(async move {
				let interval = interval(Duration::from_secs(60 * 60));
				let stream = IntervalStream::new(interval).zip(futures::stream::iter(1..=5)).map(|(_, c)| c);

				pipe_from_stream(stream, pending).await;
				let send_back = std::sync::Arc::make_mut(&mut tx);
				send_back.send(()).await.unwrap();
			});
			Ok(())
		})
		.unwrap();
	let handle = server.start(module).unwrap();

	tokio::spawn(handle.stopped());

	addr
}

#[allow(dead_code)]
pub async fn server_with_health_api() -> (SocketAddr, ServerHandle) {
	server_with_access_control(AllowHosts::Any, CorsLayer::new()).await
}

pub async fn server_with_access_control(allowed_hosts: AllowHosts, cors: CorsLayer) -> (SocketAddr, ServerHandle) {
	let middleware = tower::ServiceBuilder::new()
		// Proxy `GET /health` requests to internal `system_health` method.
		.layer(ProxyGetRequestLayer::new("/health", "system_health").unwrap())
		// Add `CORS` layer.
		.layer(cors);

	let server = ServerBuilder::default()
		.set_host_filtering(allowed_hosts)
		.set_middleware(middleware)
		.build("127.0.0.1:0")
		.await
		.unwrap();
	let mut module = RpcModule::new(());
	let addr = server.local_addr().unwrap();
	module.register_method("say_hello", |_, _| Ok("hello")).unwrap();
	module.register_method("notif", |_, _| Ok("")).unwrap();

	module.register_method("system_health", |_, _| Ok(serde_json::json!({ "health": true }))).unwrap();

	let handle = server.start(module).unwrap();
	(addr, handle)
}

pub fn init_logger() {
	let _ = tracing_subscriber::FmtSubscriber::builder()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.try_init();
}

async fn pipe_from_stream<S, T>(mut stream: S, pending: PendingSubscriptionSink)
where
	S: StreamExt<Item = T> + Unpin,
	T: Serialize,
{
	let sink = match pending.accept().await {
		Ok(s) => s,
		Err(_) => return,
	};

	loop {
		tokio::select! {
			// poll the sink first.
			biased;
			_ = sink.closed() => return,

			maybe_item = stream.next() => {
				let item = match maybe_item {
					Some(item) => item,
					None => {
						let _ = sink.close(SubscriptionClosed::Success).await;
						return;
					}
				};

				let msg = sink.build_message(&item).unwrap();

				if sink.send(msg).await.is_err() {
					return;
				}
			},

		}
	}
}
