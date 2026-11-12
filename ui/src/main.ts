import { start, testHooks, useMock } from "./app";
import "katex/dist/katex.min.css";

// `?mock=1` runs the same UI against an in-browser fake of the Rust side, for
// demos and screenshots without a model. The real app always uses IPC.
const mock = new URLSearchParams(location.search).has("mock");
if (mock) {
  useMock();
  (window as unknown as { aria: unknown }).aria = testHooks;
}

void start();
