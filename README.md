# Deno Minus V8

<img align="right" src=".github/deno_minus_v8.png" height="150px" alt="the deno dinosaur breaking up with V8">

`deno_minus_v8` is a fork of Deno's runtime that entirely removes V8.

**This is currently a work in progress.**
`deno_minus_v8` is currently missing several crucial things for
parity with upstream Deno (see the list of TODOs scattered around
the codebase). Security is likely borked and prehistoric bugs may
appear. In particular, expect Deno's Node compat to behave weirdly.

