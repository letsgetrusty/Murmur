import { describe, it, expect } from "vitest";
import { EVENTS, CMD, TABS, DOWNLOAD } from "./constants.js";

// Intra-JS integrity of the shared constants. The Rust↔JS agreement (that these
// values match the backend's events/commands) is enforced separately by the
// contract test in src-tauri/src/ipc.rs.
describe("constants integrity", () => {
  for (const [name, obj] of Object.entries({ EVENTS, CMD, TABS, DOWNLOAD })) {
    describe(name, () => {
      it("has only non-empty string values", () => {
        for (const [key, value] of Object.entries(obj)) {
          expect(typeof value, `${name}.${key}`).toBe("string");
          expect(value.length, `${name}.${key}`).toBeGreaterThan(0);
        }
      });

      it("has no duplicate values", () => {
        const values = Object.values(obj);
        expect(new Set(values).size).toBe(values.length);
      });
    });
  }

  it("TABS covers exactly the three settings tabs", () => {
    expect(new Set(Object.values(TABS))).toEqual(
      new Set(["history", "insights", "settings"])
    );
  });
});
