#![deny(clippy::all)]

mod threadsafe_function;

use std::{sync::{Mutex, Arc, atomic::{AtomicUsize, Ordering}, mpsc::Sender}};
use lazy_static::lazy_static;
use rayon::{prelude::*};
use std::sync::mpsc;

use threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode, ThreadSafeCallContext};

use napi::{
  Result,
  JsFunction, Env,
};

use napi::NapiRaw;

#[macro_use]
extern crate napi_derive;

struct Worker {
  callback: ThreadsafeFunction<Message>,
  queue_size: Arc<AtomicUsize>
}

struct Message {
  value: u32,
  tx: Sender<u32>
}

lazy_static! {
  static ref JS_THREADS: Mutex<Vec<Worker>> = Mutex::new(Vec::new());
}

#[napi]
pub fn register_worker(env: Env, callback: JsFunction) -> Result<()> {
  let tsfn: ThreadsafeFunction<Message> = ThreadsafeFunction::create(env.raw(), unsafe { callback.raw() }, 0, |ctx: ThreadSafeCallContext<Message>| {
    let value = ctx.env.create_uint32(ctx.value.value)?;
    let result = ctx.callback.call(None, &[value])?;
    let result = result.coerce_to_number()?;
    let result = result.get_uint32()?;
    ctx.value.tx.send(result).unwrap();
    Ok(())
  })?;

  JS_THREADS.lock().unwrap().push(Worker {
    callback: tsfn,
    queue_size: Arc::new(AtomicUsize::new(0))
  });

  Ok(())
}

#[napi]
pub fn do_stuff(count: u32) -> u32 {
  // Call registered worker function `count` times, in parallel.
  (0..count).into_par_iter().map(|x| {
    let (rx, queue_size) = {
      // Find worker with fewest number of active calls.
      let mut workers = JS_THREADS.lock().unwrap();
      let worker = workers.iter_mut().min_by(|a, b| a.queue_size.load(Ordering::Relaxed).cmp(&b.queue_size.load(Ordering::Relaxed))).unwrap();

      let (tx, rx) = mpsc::channel();
      let msg = Message {
        value: x,
        tx
      };

      let queue_size = Arc::clone(&worker.queue_size);
      queue_size.fetch_add(1, Ordering::Relaxed);

      worker.callback.call(Ok(msg), ThreadsafeFunctionCallMode::Blocking);
      (rx, queue_size)
    };

    let result = rx.recv().unwrap();
    queue_size.fetch_sub(1, Ordering::Relaxed);
    result
  }).sum()
}
