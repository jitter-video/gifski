// First up, but try to do feature detection to provide better error messages
function loadWasm() {
  //   let msg = "This demo requires a current version of Firefox (e.g., 79.0)";
  //   if (typeof SharedArrayBuffer !== "function") {
  //     alert(
  //       "this browser does not have SharedArrayBuffer support enabled" +
  //         "\n\n" +
  //         msg
  //     );
  //     return;
  //   }

  // Test for bulk memory operations with passive data segments
  //  (module (memory 1) (data passive ""))
  const buf = new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x00,
    0x01, 0x0b, 0x03, 0x01, 0x01, 0x00,
  ]);
  if (!WebAssembly.validate(buf)) {
    alert(
      "this browser does not support passive wasm memory, demo does not work" +
        "\n\n" +
        msg
    );
    return;
  }

  wasm_bindgen("./gifski_bg.wasm").then(run).catch(console.error);
}

loadWasm();

const { WorkerPool } = wasm_bindgen;
let pool = null;

async function run() {
  console.log(wasm_bindgen);

  // The maximal concurrency of our web worker pool is `hardwareConcurrency`,
  // so set that up here and this ideally is the only location we create web
  // workers.
  const body = document.getElementById("body");
  const canvas = document.createElement("canvas");
  canvas.style = "border: 1px solid black";

  const nbFrames = 100;
  const width = 300;
  const height = 300;
  const fps = 10;
  const quality = 100;

  canvas.width = width;
  canvas.height = height;
  body.removeChild(body.firstChild);
  body.appendChild(canvas);

  const encoder = new wasm_bindgen.Encoder();
  while (encoder.is_ready() === false) {
    console.log("waiting for encoder to be ready");
    await new Promise((r) => setTimeout(r, 100));
  }

  for (let i = 0; i < nbFrames; i++) {
    const ctx = canvas.getContext("2d");
    ctx.fillStyle = ["red", "green", "blue"][i % 3];
    ctx.fillRect(100 + i * 2, 100 + i * 2, 100, 100);
    const data = ctx.getImageData(0, 0, width, height).data.buffer;
    // u8.set(new Uint8Array(data), i * width * height * 4);
    const frame = new Uint8Array(data);
    while (encoder.get_queue_size() >= 5) {
      console.log("waiting for queue");
      await new Promise((r) => setTimeout(r, 10));
    }
    encoder.add_frame(frame, width, height, fps);
  }

  setTimeout(() => {
    encoder.close();
  }, 3000);
}
