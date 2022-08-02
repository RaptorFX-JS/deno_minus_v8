// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.
"use strict";

// Removes the `__proto__` for security reasons.  This intentionally makes
// Deno non compliant with ECMA-262 Annex B.2.2.1
delete Object.prototype.__proto__;

// Remove Intl.v8BreakIterator because it is a non-standard API.
delete Intl.v8BreakIterator;

((window) => {
  const core = Deno.core;
  const {
    ArrayPrototypeMap,
    DateNow,
    Error,
    ObjectAssign,
    ObjectDefineProperty,
    ObjectDefineProperties,
    ObjectFreeze,
    ObjectSetPrototypeOf,
    Symbol,
    SymbolFor,
    TypeError,
  } = window.__bootstrap.primordials;
  const util = window.__bootstrap.util;
  const location = window.__bootstrap.location;
  const build = window.__bootstrap.build;
  const version = window.__bootstrap.version;
  const os = window.__bootstrap.os;
  const colors = window.__bootstrap.colors;
  const internals = window.__bootstrap.internals;
  const performance = window.__bootstrap.performance;
  const url = window.__bootstrap.url;
  const denoNs = window.__bootstrap.denoNs;
  const denoNsUnstable = window.__bootstrap.denoNsUnstable;
  const errors = window.__bootstrap.errors.errors;
  const domException = window.__bootstrap.domException;
  const { defineEventHandler } = window.__bootstrap.event;

  function workerClose() {
    if (isClosing) {
      return;
    }

    isClosing = true;
    core.opSync("op_worker_close");
  }

  let isClosing = false;

  let loadedMainWorkerScript = false;

  function importScripts(...urls) {
    if (core.opSync("op_worker_get_type") === "module") {
      throw new TypeError("Can't import scripts in a module worker.");
    }

    const baseUrl = location.getLocationHref();
    const parsedUrls = ArrayPrototypeMap(urls, (scriptUrl) => {
      try {
        return new url.URL(scriptUrl, baseUrl ?? undefined).href;
      } catch {
        throw new domException.DOMException(
          "Failed to parse URL.",
          "SyntaxError",
        );
      }
    });

    // A classic worker's main script has looser MIME type checks than any
    // imported scripts, so we use `loadedMainWorkerScript` to distinguish them.
    // TODO(andreubotella) Refactor worker creation so the main script isn't
    // loaded with `importScripts()`.
    const scripts = core.opSync(
      "op_worker_sync_fetch",
      parsedUrls,
      !loadedMainWorkerScript,
    );
    loadedMainWorkerScript = true;

    for (const { url, script } of scripts) {
      const err = core.evalContext(script, url)[1];
      if (err !== null) {
        throw err.thrown;
      }
    }
  }

  function opMainModule() {
    return core.opSync("op_main_module");
  }

  // TODO(minus_v8) format exception callback
  /*
  function formatException(error) {
    if (error instanceof Error) {
      return null;
    } else if (typeof error == "string") {
      return `Uncaught ${
        inspectArgs([quoteString(error)], {
          colors: !colors.getNoColor(),
        })
      }`;
    } else {
      return `Uncaught ${
        inspectArgs([error], { colors: !colors.getNoColor() })
      }`;
    }
  }
  */

  function runtimeStart(runtimeOptions, source) {
    // TODO(minus_v8) format exception callback
    // core.opSync("op_set_format_exception_callback", formatException);
    version.setVersions(
      runtimeOptions.denoVersion,
      runtimeOptions.v8Version,
      runtimeOptions.tsVersion,
    );
    build.setBuildInfo(runtimeOptions.target);
    util.setLogDebug(runtimeOptions.debugFlag, source);
    // deno-lint-ignore prefer-primordials
    Error.prepareStackTrace = core.prepareStackTrace;
  }

  function registerErrors() {
    core.registerErrorClass("NotFound", errors.NotFound);
    core.registerErrorClass("PermissionDenied", errors.PermissionDenied);
    core.registerErrorClass("ConnectionRefused", errors.ConnectionRefused);
    core.registerErrorClass("ConnectionReset", errors.ConnectionReset);
    core.registerErrorClass("ConnectionAborted", errors.ConnectionAborted);
    core.registerErrorClass("NotConnected", errors.NotConnected);
    core.registerErrorClass("AddrInUse", errors.AddrInUse);
    core.registerErrorClass("AddrNotAvailable", errors.AddrNotAvailable);
    core.registerErrorClass("BrokenPipe", errors.BrokenPipe);
    core.registerErrorClass("AlreadyExists", errors.AlreadyExists);
    core.registerErrorClass("InvalidData", errors.InvalidData);
    core.registerErrorClass("TimedOut", errors.TimedOut);
    core.registerErrorClass("Interrupted", errors.Interrupted);
    core.registerErrorClass("WriteZero", errors.WriteZero);
    core.registerErrorClass("UnexpectedEof", errors.UnexpectedEof);
    core.registerErrorClass("BadResource", errors.BadResource);
    core.registerErrorClass("Http", errors.Http);
    core.registerErrorClass("Busy", errors.Busy);
    core.registerErrorClass("NotSupported", errors.NotSupported);
    core.registerErrorBuilder(
      "DOMExceptionOperationError",
      function DOMExceptionOperationError(msg) {
        return new domException.DOMException(msg, "OperationError");
      },
    );
    core.registerErrorBuilder(
      "DOMExceptionQuotaExceededError",
      function DOMExceptionQuotaExceededError(msg) {
        return new domException.DOMException(msg, "QuotaExceededError");
      },
    );
    core.registerErrorBuilder(
      "DOMExceptionNotSupportedError",
      function DOMExceptionNotSupportedError(msg) {
        return new domException.DOMException(msg, "NotSupported");
      },
    );
    core.registerErrorBuilder(
      "DOMExceptionNetworkError",
      function DOMExceptionNetworkError(msg) {
        return new domException.DOMException(msg, "NetworkError");
      },
    );
    core.registerErrorBuilder(
      "DOMExceptionAbortError",
      function DOMExceptionAbortError(msg) {
        return new domException.DOMException(msg, "AbortError");
      },
    );
    core.registerErrorBuilder(
      "DOMExceptionInvalidCharacterError",
      function DOMExceptionInvalidCharacterError(msg) {
        return new domException.DOMException(msg, "InvalidCharacterError");
      },
    );
    core.registerErrorBuilder(
      "DOMExceptionDataError",
      function DOMExceptionDataError(msg) {
        return new domException.DOMException(msg, "DataError");
      },
    );
  }

  let hasBootstrapped = false;

  function bootstrapMainRuntime(runtimeOptions) {
    if (hasBootstrapped) {
      throw new Error("Worker runtime already bootstrapped");
    }

    // performance.setTimeOrigin(DateNow());
    // const consoleFromV8 = window.console;
    // const wrapConsole = window.__bootstrap.console.wrapConsole;

    // Remove bootstrapping data from the global scope
    delete globalThis.__bootstrap;
    delete globalThis.bootstrap;
    util.log("bootstrapMainRuntime");
    hasBootstrapped = true;

    // const consoleFromDeno = globalThis.console;
    // wrapConsole(consoleFromDeno, consoleFromV8);

    defineEventHandler(window, "error");
    defineEventHandler(window, "load");
    defineEventHandler(window, "unload");
    const isUnloadDispatched = SymbolFor("isUnloadDispatched");
    // Stores the flag for checking whether unload is dispatched or not.
    // This prevents the recursive dispatches of unload events.
    // See https://github.com/denoland/deno/issues/9201.
    window[isUnloadDispatched] = false;
    window.addEventListener("unload", () => {
      window[isUnloadDispatched] = true;
    });

    runtimeStart(runtimeOptions);
    const {
      args,
      location: locationHref,
      noColor,
      isTty,
      pid,
      ppid,
      unstableFlag,
    } = runtimeOptions;

    colors.setNoColor(noColor || !isTty);
    if (locationHref != null) {
      location.setLocationHref(locationHref);
    }
    registerErrors();

    const internalSymbol = Symbol("Deno.internal");

    const finalDenoNs = {
      core,
      internal: internalSymbol,
      [internalSymbol]: internals,
      resources: core.resources,
      close: core.close,
      ...denoNs,
    };
    ObjectDefineProperties(finalDenoNs, {
      pid: util.readOnly(pid),
      ppid: util.readOnly(ppid),
      noColor: util.readOnly(noColor),
      args: util.readOnly(ObjectFreeze(args)),
      mainModule: util.getterOnly(opMainModule),
    });

    if (unstableFlag) {
      ObjectAssign(finalDenoNs, denoNsUnstable);
    }

    // Setup `Deno` global - we're actually overriding already existing global
    // `Deno` with `Deno` namespace from "./deno.ts".
    ObjectDefineProperty(globalThis, "Deno", util.readOnly(finalDenoNs));
    ObjectFreeze(globalThis.Deno.core);

    util.log("args", args);
  }

  function bootstrapWorkerRuntime(
    runtimeOptions,
    name,
    internalName,
  ) {
    if (hasBootstrapped) {
      throw new Error("Worker runtime already bootstrapped");
    }

    performance.setTimeOrigin(DateNow());
    const consoleFromV8 = window.console;
    const wrapConsole = window.__bootstrap.console.wrapConsole;

    // Remove bootstrapping data from the global scope
    delete globalThis.__bootstrap;
    delete globalThis.bootstrap;
    util.log("bootstrapWorkerRuntime");
    hasBootstrapped = true;
    ObjectDefineProperties(globalThis, windowOrWorkerGlobalScope);
    if (runtimeOptions.unstableFlag) {
      ObjectDefineProperties(globalThis, unstableWindowOrWorkerGlobalScope);
    }
    ObjectDefineProperties(globalThis, workerRuntimeGlobalProperties);
    ObjectDefineProperties(globalThis, { name: util.writable(name) });
    if (runtimeOptions.enableTestingFeaturesFlag) {
      ObjectDefineProperty(
        globalThis,
        "importScripts",
        util.writable(importScripts),
      );
    }
    ObjectSetPrototypeOf(globalThis, DedicatedWorkerGlobalScope.prototype);

    const consoleFromDeno = globalThis.console;
    wrapConsole(consoleFromDeno, consoleFromV8);

    // eventTarget.setEventTargetData(globalThis);

    defineEventHandler(self, "message");
    defineEventHandler(self, "error", undefined, true);
    // `Deno.exit()` is an alias to `self.close()`. Setting and exit
    // code using an op in worker context is a no-op.
    os.setExitHandler((_exitCode) => {
      workerClose();
    });

    runtimeStart(
      runtimeOptions,
      internalName ?? name,
    );
    const {
      unstableFlag,
      pid,
      noColor,
      isTty,
      args,
      location: locationHref,
    } = runtimeOptions;

    colors.setNoColor(noColor || !isTty);
    location.setLocationHref(locationHref);
    registerErrors();

    const internalSymbol = Symbol("Deno.internal");

    const finalDenoNs = {
      core,
      internal: internalSymbol,
      [internalSymbol]: internals,
      resources: core.resources,
      close: core.close,
      ...denoNs,
    };
    if (unstableFlag) {
      ObjectAssign(finalDenoNs, denoNsUnstable);
    }
    ObjectDefineProperties(finalDenoNs, {
      pid: util.readOnly(pid),
      noColor: util.readOnly(noColor),
      args: util.readOnly(ObjectFreeze(args)),
    });
    // Setup `Deno` global - we're actually overriding already
    // existing global `Deno` with `Deno` namespace from "./deno.ts".
    ObjectDefineProperty(globalThis, "Deno", util.readOnly(finalDenoNs));
    ObjectFreeze(globalThis.Deno.core);
  }

  ObjectDefineProperties(globalThis, {
    bootstrap: {
      value: {
        mainRuntime: bootstrapMainRuntime,
        workerRuntime: bootstrapWorkerRuntime,
      },
      configurable: true,
    },
  });
})(this);
