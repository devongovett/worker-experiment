// Fork of threadsafe_function from napi-rs that allows calling JS function manually rather than
// only returning args. This enables us to use the return value of the function.

#![allow(clippy::single_component_path_imports)]

use std::convert::Into;
use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use napi::{JsFunction, NapiValue};
use napi::{check_status, sys, Env, Result, Status, threadsafe_function::ErrorStrategy};

/// ThreadSafeFunction Context object
/// the `value` is the value passed to `call` method
pub struct ThreadSafeCallContext<T: 'static> {
  pub env: Env,
  pub value: T,
  pub callback: JsFunction
}

#[repr(u8)]
pub enum ThreadsafeFunctionCallMode {
  NonBlocking,
  Blocking,
}

impl From<ThreadsafeFunctionCallMode> for sys::napi_threadsafe_function_call_mode {
  fn from(value: ThreadsafeFunctionCallMode) -> Self {
    match value {
      ThreadsafeFunctionCallMode::Blocking => sys::ThreadsafeFunctionCallMode::blocking,
      ThreadsafeFunctionCallMode::NonBlocking => sys::ThreadsafeFunctionCallMode::nonblocking,
    }
  }
}

/// Communicate with the addon's main thread by invoking a JavaScript function from other threads.
///
/// ## Example
/// An example of using `ThreadsafeFunction`:
///
/// ```rust
/// #[macro_use]
/// extern crate napi_derive;
///
/// use std::thread;
///
/// use napi::{
///     threadsafe_function::{
///         ThreadSafeCallContext, ThreadsafeFunctionCallMode, ThreadsafeFunctionReleaseMode,
///     },
///     CallContext, Error, JsFunction, JsNumber, JsUndefined, Result, Status,
/// };
///
/// #[js_function(1)]
/// pub fn test_threadsafe_function(ctx: CallContext) -> Result<JsUndefined> {
///   let func = ctx.get::<JsFunction>(0)?;
///
///   let tsfn =
///       ctx
///           .env
///           .create_threadsafe_function(&func, 0, |ctx: ThreadSafeCallContext<Vec<u32>>| {
///             ctx.value
///                 .iter()
///                 .map(|v| ctx.env.create_uint32(*v))
///                 .collect::<Result<Vec<JsNumber>>>()
///           })?;
///
///   let tsfn_cloned = tsfn.clone();
///
///   thread::spawn(move || {
///       let output: Vec<u32> = vec![0, 1, 2, 3];
///       // It's okay to call a threadsafe function multiple times.
///       tsfn.call(Ok(output.clone()), ThreadsafeFunctionCallMode::Blocking);
///   });
///
///   thread::spawn(move || {
///       let output: Vec<u32> = vec![3, 2, 1, 0];
///       // It's okay to call a threadsafe function multiple times.
///       tsfn_cloned.call(Ok(output.clone()), ThreadsafeFunctionCallMode::NonBlocking);
///   });
///
///   ctx.env.get_undefined()
/// }
/// ```
pub struct ThreadsafeFunction<T: 'static, ES: ErrorStrategy::T = ErrorStrategy::CalleeHandled> {
  raw_tsfn: sys::napi_threadsafe_function,
  aborted: Arc<AtomicBool>,
  ref_count: Arc<AtomicUsize>,
  _phantom: PhantomData<(T, ES)>,
}

impl<T: 'static, ES: ErrorStrategy::T> Clone for ThreadsafeFunction<T, ES> {
  fn clone(&self) -> Self {
    if !self.aborted.load(Ordering::Acquire) {
      let acquire_status = unsafe { sys::napi_acquire_threadsafe_function(self.raw_tsfn) };
      debug_assert!(
        acquire_status == sys::Status::napi_ok,
        "Acquire threadsafe function failed in clone"
      );
    }

    Self {
      raw_tsfn: self.raw_tsfn,
      aborted: Arc::clone(&self.aborted),
      ref_count: Arc::clone(&self.ref_count),
      _phantom: PhantomData,
    }
  }
}

unsafe impl<T, ES: ErrorStrategy::T> Send for ThreadsafeFunction<T, ES> {}
unsafe impl<T, ES: ErrorStrategy::T> Sync for ThreadsafeFunction<T, ES> {}

impl<T: 'static, ES: ErrorStrategy::T> ThreadsafeFunction<T, ES> {
  /// See [napi_create_threadsafe_function](https://nodejs.org/api/n-api.html#n_api_napi_create_threadsafe_function)
  /// for more information.
  pub(crate) fn create<
    R: 'static + Send + FnMut(ThreadSafeCallContext<T>) -> Result<()>,
  >(
    env: sys::napi_env,
    func: sys::napi_value,
    max_queue_size: usize,
    callback: R,
  ) -> Result<Self> {
    let mut async_resource_name = ptr::null_mut();
    let s = "napi_rs_threadsafe_function";
    let len = s.len();
    let s = CString::new(s)?;
    check_status!(unsafe {
      sys::napi_create_string_utf8(env, s.as_ptr(), len, &mut async_resource_name)
    })?;

    let initial_thread_count = 1usize;
    let mut raw_tsfn = ptr::null_mut();
    let ptr = Box::into_raw(Box::new(callback)) as *mut c_void;
    check_status!(unsafe {
      sys::napi_create_threadsafe_function(
        env,
        func,
        ptr::null_mut(),
        async_resource_name,
        max_queue_size,
        initial_thread_count,
        ptr,
        Some(thread_finalize_cb::<T, R>),
        ptr,
        Some(call_js_cb::<T, R, ES>),
        &mut raw_tsfn,
      )
    })?;

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_ptr = Arc::into_raw(aborted.clone()) as *mut c_void;
    check_status!(unsafe { sys::napi_add_env_cleanup_hook(env, Some(cleanup_cb), aborted_ptr) })?;

    Ok(ThreadsafeFunction {
      raw_tsfn,
      aborted,
      ref_count: Arc::new(AtomicUsize::new(initial_thread_count)),
      _phantom: PhantomData,
    })
  }

  pub fn aborted(&self) -> bool {
    self.aborted.load(Ordering::Relaxed)
  }

  pub fn abort(self) -> Result<()> {
    check_status!(unsafe {
      sys::napi_release_threadsafe_function(
        self.raw_tsfn,
        sys::ThreadsafeFunctionReleaseMode::abort,
      )
    })?;
    self.aborted.store(true, Ordering::Release);
    Ok(())
  }

  /// Get the raw `ThreadSafeFunction` pointer
  pub fn raw(&self) -> sys::napi_threadsafe_function {
    self.raw_tsfn
  }
}

impl<T: 'static> ThreadsafeFunction<T, ErrorStrategy::CalleeHandled> {
  /// See [napi_call_threadsafe_function](https://nodejs.org/api/n-api.html#n_api_napi_call_threadsafe_function)
  /// for more information.
  pub fn call(&self, value: Result<T>, mode: ThreadsafeFunctionCallMode) -> Status {
    if self.aborted.load(Ordering::Acquire) {
      return Status::Closing;
    }
    unsafe {
      sys::napi_call_threadsafe_function(
        self.raw_tsfn,
        Box::into_raw(Box::new(value)) as *mut _,
        mode.into(),
      )
    }
    .into()
  }
}

impl<T: 'static> ThreadsafeFunction<T, ErrorStrategy::Fatal> {
  /// See [napi_call_threadsafe_function](https://nodejs.org/api/n-api.html#n_api_napi_call_threadsafe_function)
  /// for more information.
  pub fn call(&self, value: T, mode: ThreadsafeFunctionCallMode) -> Status {
    if self.aborted.load(Ordering::Acquire) {
      return Status::Closing;
    }
    unsafe {
      sys::napi_call_threadsafe_function(
        self.raw_tsfn,
        Box::into_raw(Box::new(value)) as *mut _,
        mode.into(),
      )
    }
    .into()
  }
}

impl<T: 'static, ES: ErrorStrategy::T> Drop for ThreadsafeFunction<T, ES> {
  fn drop(&mut self) {
    if !self.aborted.load(Ordering::Acquire) && self.ref_count.load(Ordering::Acquire) > 0usize {
      let release_status = unsafe {
        sys::napi_release_threadsafe_function(
          self.raw_tsfn,
          sys::ThreadsafeFunctionReleaseMode::release,
        )
      };
      assert!(
        release_status == sys::Status::napi_ok,
        "Threadsafe Function release failed"
      );
    }
  }
}

unsafe extern "C" fn cleanup_cb(cleanup_data: *mut c_void) {
  let aborted = unsafe { Arc::<AtomicBool>::from_raw(cleanup_data.cast()) };
  aborted.store(true, Ordering::SeqCst);
}

unsafe extern "C" fn thread_finalize_cb<T: 'static, R>(
  _raw_env: sys::napi_env,
  finalize_data: *mut c_void,
  _finalize_hint: *mut c_void,
) where
  R: 'static + Send + FnMut(ThreadSafeCallContext<T>) -> Result<()>,
{
  // cleanup
  drop(unsafe { Box::<R>::from_raw(finalize_data.cast()) });
}

unsafe extern "C" fn call_js_cb<T: 'static, R, ES>(
  raw_env: sys::napi_env,
  js_callback: sys::napi_value,
  context: *mut c_void,
  data: *mut c_void,
) where
  R: 'static + Send + FnMut(ThreadSafeCallContext<T>) -> Result<()>,
  ES: ErrorStrategy::T,
{
  // env and/or callback can be null when shutting down
  if raw_env.is_null() || js_callback.is_null() {
    return;
  }

  let ctx: &mut R = unsafe { &mut *context.cast::<R>() };
  let val: Result<T> = unsafe {
    match ES::VALUE {
      ErrorStrategy::CalleeHandled::VALUE => *Box::<Result<T>>::from_raw(data.cast()),
      ErrorStrategy::Fatal::VALUE => Ok(*Box::<T>::from_raw(data.cast())),
    }
  };

  let mut recv = ptr::null_mut();
  unsafe { sys::napi_get_undefined(raw_env, &mut recv) };

  let ret = val.and_then(|v| {
    (ctx)(ThreadSafeCallContext {
      env: unsafe { Env::from_raw(raw_env) },
      value: v,
      callback: JsFunction::from_raw(
        raw_env,
        js_callback
      ).unwrap() // TODO: unwrap
    })
  });

  let status = sys::Status::napi_ok;
  // TODO: error handling

  // Follow async callback conventions: https://nodejs.org/en/knowledge/errors/what-are-the-error-conventions/
  // Check if the Result is okay, if so, pass a null as the first (error) argument automatically.
  // If the Result is an error, pass that as the first argument.
  // let status = match ret {
  //   Ok(values) => {
  //     let values = values
  //       .into_iter()
  //       .map(|v| unsafe { ToNapiValue::to_napi_value(raw_env, v) });
  //     let args: Result<Vec<sys::napi_value>> = if ES::VALUE == ErrorStrategy::CalleeHandled::VALUE {
  //       let mut js_null = ptr::null_mut();
  //       unsafe { sys::napi_get_null(raw_env, &mut js_null) };
  //       ::core::iter::once(Ok(js_null)).chain(values).collect()
  //     } else {
  //       values.collect()
  //     };
  //     match args {
  //       Ok(args) => unsafe {
  //         sys::napi_call_function(
  //           raw_env,
  //           recv,
  //           js_callback,
  //           args.len(),
  //           args.as_ptr(),
  //           ptr::null_mut(),
  //         )
  //       },
  //       Err(e) => match ES::VALUE {
  //         ErrorStrategy::Fatal::VALUE => unsafe {
  //           sys::napi_fatal_exception(raw_env, JsError::from(e).into_value(raw_env))
  //         },
  //         ErrorStrategy::CalleeHandled::VALUE => unsafe {
  //           sys::napi_call_function(
  //             raw_env,
  //             recv,
  //             js_callback,
  //             1,
  //             [JsError::from(e).into_value(raw_env)].as_mut_ptr(),
  //             ptr::null_mut(),
  //           )
  //         },
  //       },
  //     }
  //   }
  //   Err(e) if ES::VALUE == ErrorStrategy::Fatal::VALUE => unsafe {
  //     sys::napi_fatal_exception(raw_env, JsError::from(e).into_value(raw_env))
  //   },
  //   Err(e) => unsafe {
  //     sys::napi_call_function(
  //       raw_env,
  //       recv,
  //       js_callback,
  //       1,
  //       [JsError::from(e).into_value(raw_env)].as_mut_ptr(),
  //       ptr::null_mut(),
  //     )
  //   },
  // };
  if status == sys::Status::napi_ok {
    return;
  }
  if status == sys::Status::napi_pending_exception {
    let mut error_result = ptr::null_mut();
    assert_eq!(
      unsafe { sys::napi_get_and_clear_last_exception(raw_env, &mut error_result) },
      sys::Status::napi_ok
    );

    // When shutting down, napi_fatal_exception sometimes returns another exception
    let stat = unsafe { sys::napi_fatal_exception(raw_env, error_result) };
    assert!(stat == sys::Status::napi_ok || stat == sys::Status::napi_pending_exception);
  } else {
    let error_code: Status = status.into();
    let error_code_string = format!("{:?}", error_code);
    let mut error_code_value = ptr::null_mut();
    assert_eq!(
      unsafe {
        sys::napi_create_string_utf8(
          raw_env,
          error_code_string.as_ptr() as *const _,
          error_code_string.len(),
          &mut error_code_value,
        )
      },
      sys::Status::napi_ok,
    );
    let error_msg = "Call JavaScript callback failed in thread safe function";
    let mut error_msg_value = ptr::null_mut();
    assert_eq!(
      unsafe {
        sys::napi_create_string_utf8(
          raw_env,
          error_msg.as_ptr() as *const _,
          error_msg.len(),
          &mut error_msg_value,
        )
      },
      sys::Status::napi_ok,
    );
    let mut error_value = ptr::null_mut();
    assert_eq!(
      unsafe {
        sys::napi_create_error(raw_env, error_code_value, error_msg_value, &mut error_value)
      },
      sys::Status::napi_ok,
    );
    assert_eq!(
      unsafe { sys::napi_fatal_exception(raw_env, error_value) },
      sys::Status::napi_ok
    );
  }
}
