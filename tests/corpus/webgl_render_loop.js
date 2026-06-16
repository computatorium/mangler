// WebGL-shaped regression fixture (whole-program §9.3): a single top-level IIFE
// that drives a stubbed GL context and a render loop, recording the GL call
// SEQUENCE into globalThis.__out. Self-contained (a fake GL context, no real
// canvas), so it runs under the in-process rquickjs harness without a DOM. This is
// the one-top-level-IIFE shape `--virtualize-program` must virtualize end-to-end.
(function () {
  const __gl = [];

  // A stubbed WebGL rendering context: every entry point records its call.
  function makeGL() {
    const C = {
      COLOR_BUFFER_BIT: 0x4000,
      DEPTH_BUFFER_BIT: 0x0100,
      TRIANGLES: 0x0004,
      ARRAY_BUFFER: 0x8892,
      STATIC_DRAW: 0x88e4,
      clearColor(r, g, b, a) {
        __gl.push(["clearColor", r, g, b, a]);
      },
      clear(mask) {
        __gl.push(["clear", mask]);
      },
      createBuffer() {
        __gl.push(["createBuffer"]);
        return { id: __gl.length };
      },
      bindBuffer(target, buf) {
        __gl.push(["bindBuffer", target, buf.id]);
      },
      bufferData(target, data, usage) {
        __gl.push(["bufferData", target, data.length, usage]);
      },
      drawArrays(mode, first, count) {
        __gl.push(["drawArrays", mode, first, count]);
      },
      uniform1f(loc, v) {
        __gl.push(["uniform1f", loc, v]);
      },
    };
    return C;
  }

  const gl = makeGL();

  // One-time setup: upload a triangle.
  const verts = [0, 1, -1, -1, 1, -1];
  const buf = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, buf);
  gl.bufferData(gl.ARRAY_BUFFER, verts, gl.STATIC_DRAW);

  // A render loop (driven synchronously here instead of requestAnimationFrame so
  // the fixture is deterministic and self-contained).
  function renderFrame(t) {
    gl.clearColor(0.1 * t, 0.2, 0.3, 1.0);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    gl.uniform1f("u_time", t);
    gl.drawArrays(gl.TRIANGLES, 0, verts.length / 2);
  }

  for (let frame = 0; frame < 4; frame++) {
    renderFrame(frame);
  }

  globalThis.__out = JSON.stringify(__gl);
})();
