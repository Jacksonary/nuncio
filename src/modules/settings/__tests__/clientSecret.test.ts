import { describe, expect, it } from "vitest";
import {
  CLIENT_SECRET_MASK,
  clientSecretPatch,
  hasStoredClientSecret,
} from "../lib/clientSecret";

describe("Google client secret masking", () => {
  it("recognizes the mask returned for a stored secret", () => {
    expect(hasStoredClientSecret(CLIENT_SECRET_MASK)).toBe(true);
    expect(hasStoredClientSecret(null)).toBe(false);
    expect(hasStoredClientSecret(undefined)).toBe(false);
    expect(hasStoredClientSecret("")).toBe(false);
    expect(hasStoredClientSecret("GOCSPX-secret")).toBe(false);
  });

  it("omits the field when the user did not type anything", () => {
    expect(clientSecretPatch("")).toBeUndefined();
    expect(clientSecretPatch("   ")).toBeUndefined();
    // The mask is never sent back; unchanged means "no patch field".
    expect(clientSecretPatch(CLIENT_SECRET_MASK)).toBeUndefined();
  });

  it("sends a trimmed replacement when the user typed one", () => {
    expect(clientSecretPatch("  GOCSPX-new-secret  ")).toBe("GOCSPX-new-secret");
  });
});
