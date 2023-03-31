// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
const core = globalThis.Deno.core;
const ops = core.ops;

// warn(minus_v8): signature changed since we don't have fast buffers
function consoleSize() {
  let size = ops.op_console_size();
  return { columns: size[0], rows: size[1] };
}

// warn(minus_v8): signature changed since we don't have fast buffers
function isatty(rid) {
  return !!ops.op_isatty(rid);
}

export { consoleSize, isatty };
