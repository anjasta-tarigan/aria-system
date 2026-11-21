// Unit tests for the pure settings validators in api.ts.
// Run with: node --test ui/src/api.test.ts  (Node 22+ strips types natively)
// or:      npx tsx --test ui/src/api.test.ts
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  normalizeEndpoint,
  isValidEndpoint,
  maskKey,
  dedupeModelIds,
  parseModelList,
  validateProviderForm,
  validateEmbeddingForm,
} from "../src/api.ts";

describe("normalizeEndpoint", () => {
  it("strips trailing slashes", () => {
    assert.equal(normalizeEndpoint("https://api.example.com/v1/"), "https://api.example.com/v1");
    assert.equal(normalizeEndpoint("https://api.example.com/v1//"), "https://api.example.com/v1");
    assert.equal(normalizeEndpoint("http://localhost:8080/"), "http://localhost:8080");
  });
  it("is idempotent", () => {
    assert.equal(normalizeEndpoint("https://api.example.com/v1"), "https://api.example.com/v1");
  });
  it("trims whitespace", () => {
    assert.equal(normalizeEndpoint("  https://api.example.com/v1  "), "https://api.example.com/v1");
  });
});

describe("isValidEndpoint", () => {
  it("accepts valid https URLs", () => {
    assert.equal(isValidEndpoint("https://api.openai.com/v1"), true);
    assert.equal(isValidEndpoint("https://api.example.com"), true);
    assert.equal(isValidEndpoint("https://localhost:3000"), true);
  });
  it("accepts http URLs for localhost and 127.0.0.1", () => {
    assert.equal(isValidEndpoint("http://localhost:8080"), true);
    assert.equal(isValidEndpoint("http://127.0.0.1:3000"), true);
    assert.equal(isValidEndpoint("http://[::1]:8080"), true);
  });
  it("rejects non-https/http protocols", () => {
    assert.equal(isValidEndpoint("ftp://example.com"), false);
    assert.equal(isValidEndpoint("wss://example.com"), false);
  });
  it("rejects http URLs for non-localhost", () => {
    assert.equal(isValidEndpoint("http://example.com"), false);
    assert.equal(isValidEndpoint("http://api.example.com"), false);
  });
  it("accepts http URLs for private LAN addresses", () => {
    assert.equal(isValidEndpoint("http://192.168.1.100:8888"), true);
    assert.equal(isValidEndpoint("http://10.0.0.5:8080"), true);
    assert.equal(isValidEndpoint("http://172.16.0.1"), true);
    assert.equal(isValidEndpoint("http://172.31.255.255"), true);
    assert.equal(isValidEndpoint("http://host.local"), true);
  });
  it("rejects invalid URLs", () => {
    assert.equal(isValidEndpoint("not-a-url"), false);
    assert.equal(isValidEndpoint(""), false);
    assert.equal(isValidEndpoint("https://"), false);
  });
  it("trims input", () => {
    assert.equal(isValidEndpoint("  https://api.example.com  "), true);
    assert.equal(isValidEndpoint("http://localhost  "), true);
  });
});

describe("maskKey", () => {
  it("masks short keys as ••••", () => {
    assert.equal(maskKey("abc"), "••••");
    assert.equal(maskKey("ab"), "••••");
    assert.equal(maskKey("a"), "••••");
    assert.equal(maskKey(""), "••••");
  });
  it("masks normal keys with first 2 and last 4 chars visible", () => {
    assert.equal(maskKey("sk-1234567890abcdef"), "sk••••cdef");
    assert.equal(maskKey("1234567890123456"), "12••••3456");
  });
  it("trims input", () => {
    assert.equal(maskKey("  sk-1234567890abcdef  "), "sk••••cdef");
  });
});

describe("dedupeModelIds", () => {
  it("removes empty strings", () => {
    assert.deepEqual(dedupeModelIds(["", "  ", "model1"]), ["model1"]);
    assert.deepEqual(dedupeModelIds([""]), []);
  });
  it("removes duplicates case-sensitively", () => {
    assert.deepEqual(dedupeModelIds(["model1", "model1"]), ["model1"]);
    assert.deepEqual(dedupeModelIds(["Model1", "model1"]), ["Model1", "model1"]);
  });
  it("trims each id", () => {
    assert.deepEqual(dedupeModelIds([" model1 ", "model2", " model1 "]), ["model1", "model2"]);
  });
  it("returns empty array for empty input", () => {
    assert.deepEqual(dedupeModelIds([]), []);
  });
  it("preserves order of first occurrence", () => {
    assert.deepEqual(dedupeModelIds(["b", "a", "b", "c"]), ["b", "a", "c"]);
  });
});

describe("parseModelList", () => {
  it("splits on newlines", () => {
    assert.deepEqual(parseModelList("model1\nmodel2\nmodel3"), ["model1", "model2", "model3"]);
  });
  it("splits on commas", () => {
    assert.deepEqual(parseModelList("model1,model2,model3"), ["model1", "model2", "model3"]);
  });
  it("splits on semicolons", () => {
    assert.deepEqual(parseModelList("model1;model2;model3"), ["model1", "model2", "model3"]);
  });
  it("splits on mixed separators", () => {
    assert.deepEqual(parseModelList("model1, model2\nmodel3;model4"), [
      "model1",
      "model2",
      "model3",
      "model4",
    ]);
  });
  it("trims each id and drops empty", () => {
    assert.deepEqual(parseModelList("model1, , model2\n , model3"), ["model1", "model2", "model3"]);
  });
  it("handles empty input", () => {
    assert.deepEqual(parseModelList(""), []);
    assert.deepEqual(parseModelList("   "), []);
  });
});

describe("validateProviderForm", () => {
  it("returns empty object for valid form", () => {
    assert.deepEqual(
      validateProviderForm({
        name: "Groq Prod",
        endpointUrl: "https://api.groq.com/openai/v1",
        apiKey: "sk-test",
        modelIds: ["llama-3.1-70b"],
      }),
      {},
    );
  });
  it("rejects name shorter than 2 characters", () => {
    assert.deepEqual(
      validateProviderForm({ name: "a", endpointUrl: "https://x.test", apiKey: "k", modelIds: ["m"] }),
      { name: "Provider name must be at least 2 characters." },
    );
  });
  it("rejects name longer than 50 characters", () => {
    assert.deepEqual(
      validateProviderForm({
        name: "a".repeat(51),
        endpointUrl: "https://x.test",
        apiKey: "k",
        modelIds: ["m"],
      }),
      { name: "Provider name must be 50 characters or fewer." },
    );
  });
  it("rejects empty endpointUrl", () => {
    assert.deepEqual(
      validateProviderForm({ name: "Good", endpointUrl: "", apiKey: "k", modelIds: ["m"] }),
      { endpointUrl: "Enter the endpoint base URL." },
    );
  });
  it("rejects invalid endpointUrl", () => {
    assert.deepEqual(
      validateProviderForm({
        name: "Good",
        endpointUrl: "http://example.com",
        apiKey: "k",
        modelIds: ["m"],
      }),
      { endpointUrl: "Enter a valid URL starting with https:// (http://localhost allowed)." },
    );
  });
  it("rejects empty modelIds even when apiKey is present", () => {
    assert.deepEqual(
      validateProviderForm({ name: "Good", endpointUrl: "https://x.test", apiKey: "k", modelIds: [] }),
      { modelIds: "Add at least one model id." },
    );
  });
  it("rejects more than 50 modelIds", () => {
    const modelIds = Array.from({ length: 51 }, (_, i) => `m${i}`);
    assert.deepEqual(
      validateProviderForm({ name: "Good", endpointUrl: "https://x.test", apiKey: "k", modelIds }),
      { modelIds: "A provider can have at most 50 models." },
    );
  });
  it("trims all string fields", () => {
    assert.deepEqual(
      validateProviderForm({
        name: "  Good  ",
        endpointUrl: "  https://x.test  ",
        apiKey: " k ",
        modelIds: ["m"],
      }),
      {},
    );
  });
});

describe("validateEmbeddingForm", () => {
  it("returns empty object for valid form", () => {
    assert.deepEqual(
      validateEmbeddingForm({
        model: "text-embedding-3-small",
        dimensions: 1536,
      }),
      {},
    );
  });
  it("rejects empty model", () => {
    assert.deepEqual(
      validateEmbeddingForm({ model: "" }),
      { model: "Choose or enter an embedding model." },
    );
  });
  it("rejects dimensions out of range", () => {
    assert.deepEqual(
      validateEmbeddingForm({ model: "m", dimensions: 63 }),
      { dimensions: "Dimensions must be a whole number between 64 and 8192." },
    );
    assert.deepEqual(
      validateEmbeddingForm({ model: "m", dimensions: 8193 }),
      { dimensions: "Dimensions must be a whole number between 64 and 8192." },
    );
  });
  it("accepts dimensions at boundaries", () => {
    assert.deepEqual(
      validateEmbeddingForm({ model: "m", dimensions: 64 }),
      {},
    );
    assert.deepEqual(
      validateEmbeddingForm({ model: "m", dimensions: 8192 }),
      {},
    );
  });
  it("rejects non-integer dimensions", () => {
    assert.deepEqual(
      validateEmbeddingForm({ model: "m", dimensions: 1536.5 }),
      { dimensions: "Dimensions must be a whole number between 64 and 8192." },
    );
  });
});
