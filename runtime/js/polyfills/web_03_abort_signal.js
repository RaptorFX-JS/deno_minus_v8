const {
  ObjectAssign,
  Set,
  SetPrototypeAdd,
  SetPrototypeDelete,
  Symbol,
} = globalThis.__bootstrap.primordials;

const add = Symbol("[[add]]");
const signalAbort = Symbol("[[signalAbort]]");
const remove = Symbol("[[remove]]");
const abortAlgos = Symbol("[[abortAlgos]]");

window.AbortController = new Proxy(window.AbortController, {
  construct: (target, args) => {
    const controller = new target(...args);
    const signal = controller.signal;
    ObjectAssign(signal, {
      [abortAlgos]: null,

      [add](algorithm) {
        if (this.aborted) {
          return;
        }
        if (this[abortAlgos] === null) {
          this[abortAlgos] = new Set();
        }
        SetPrototypeAdd(this[abortAlgos], algorithm);
      },

      [signalAbort](
        reason = new DOMException("The signal has been aborted", "AbortError"),
      ) {
        controller.abort(reason);
      },

      [remove](algorithm) {
        this[abortAlgos] && SetPrototypeDelete(this[abortAlgos], algorithm);
      },
    });
    signal.addEventListener("abort", () => {
      if (signal[abortAlgos] !== null) {
        for (const algorithm of signal[abortAlgos]) {
          algorithm();
        }
        signal[abortAlgos] = null;
      }
    });
    return controller;
  }
});

function newSignal() {
  return new window.AbortController().signal;
}

function follow(followingSignal, parentSignal) {
  if (followingSignal.aborted) {
    return;
  }
  if (parentSignal.aborted) {
    followingSignal[signalAbort](parentSignal.reason);
  } else {
    parentSignal[add](() =>
      followingSignal[signalAbort](parentSignal.reason)
    );
  }
}

const AbortSignalPrototype = window.AbortSignal.prototype;

export {
  AbortSignalPrototype,
  add,
  signalAbort,
  remove,
  follow,
  newSignal,
};
