// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

import * as util from "internal:runtime/js/06_util.js";
// import * as webgpu from "internal:deno_webgpu/01_webgpu.js";
import * as webSocket from "internal:deno_websocket/01_websocket.js";
import * as webSocketStream from "internal:deno_websocket/02_websocketstream.js";
import * as request from "internal:deno_fetch/23_request.js";
import * as response from "internal:deno_fetch/23_response.js";
import * as fetch from "internal:deno_fetch/26_fetch.js";
// import * as webidl from "internal:deno_webidl/00_webidl.js";

// https://developer.mozilla.org/en-US/docs/Web/API/WindowOrWorkerGlobalScope
const windowOrWorkerGlobalScope = {
  Request: util.nonEnumerable(request.Request),
  Response: util.nonEnumerable(response.Response),
  WebSocket: util.nonEnumerable(webSocket.WebSocket),
  fetch: util.writable(fetch.fetch),
  // Branding as a WebIDL object
  // [webidl.brand]: util.nonEnumerable(webidl.brand),
};

const unstableWindowOrWorkerGlobalScope = {
  WebSocketStream: util.nonEnumerable(webSocketStream.WebSocketStream),

  /*
  GPU: util.nonEnumerable(webgpu.GPU),
  GPUAdapter: util.nonEnumerable(webgpu.GPUAdapter),
  GPUAdapterInfo: util.nonEnumerable(webgpu.GPUAdapterInfo),
  GPUSupportedLimits: util.nonEnumerable(webgpu.GPUSupportedLimits),
  GPUSupportedFeatures: util.nonEnumerable(webgpu.GPUSupportedFeatures),
  GPUDeviceLostInfo: util.nonEnumerable(webgpu.GPUDeviceLostInfo),
  GPUDevice: util.nonEnumerable(webgpu.GPUDevice),
  GPUQueue: util.nonEnumerable(webgpu.GPUQueue),
  GPUBuffer: util.nonEnumerable(webgpu.GPUBuffer),
  GPUBufferUsage: util.nonEnumerable(webgpu.GPUBufferUsage),
  GPUMapMode: util.nonEnumerable(webgpu.GPUMapMode),
  GPUTexture: util.nonEnumerable(webgpu.GPUTexture),
  GPUTextureUsage: util.nonEnumerable(webgpu.GPUTextureUsage),
  GPUTextureView: util.nonEnumerable(webgpu.GPUTextureView),
  GPUSampler: util.nonEnumerable(webgpu.GPUSampler),
  GPUBindGroupLayout: util.nonEnumerable(webgpu.GPUBindGroupLayout),
  GPUPipelineLayout: util.nonEnumerable(webgpu.GPUPipelineLayout),
  GPUBindGroup: util.nonEnumerable(webgpu.GPUBindGroup),
  GPUShaderModule: util.nonEnumerable(webgpu.GPUShaderModule),
  GPUShaderStage: util.nonEnumerable(webgpu.GPUShaderStage),
  GPUComputePipeline: util.nonEnumerable(webgpu.GPUComputePipeline),
  GPURenderPipeline: util.nonEnumerable(webgpu.GPURenderPipeline),
  GPUColorWrite: util.nonEnumerable(webgpu.GPUColorWrite),
  GPUCommandEncoder: util.nonEnumerable(webgpu.GPUCommandEncoder),
  GPURenderPassEncoder: util.nonEnumerable(webgpu.GPURenderPassEncoder),
  GPUComputePassEncoder: util.nonEnumerable(webgpu.GPUComputePassEncoder),
  GPUCommandBuffer: util.nonEnumerable(webgpu.GPUCommandBuffer),
  GPURenderBundleEncoder: util.nonEnumerable(webgpu.GPURenderBundleEncoder),
  GPURenderBundle: util.nonEnumerable(webgpu.GPURenderBundle),
  GPUQuerySet: util.nonEnumerable(webgpu.GPUQuerySet),
  GPUError: util.nonEnumerable(webgpu.GPUError),
  GPUOutOfMemoryError: util.nonEnumerable(webgpu.GPUOutOfMemoryError),
  GPUValidationError: util.nonEnumerable(webgpu.GPUValidationError),
  */
};

export {
  unstableWindowOrWorkerGlobalScope,
  windowOrWorkerGlobalScope,
};
