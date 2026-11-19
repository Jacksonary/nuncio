import { beforeEach, describe, expect, it } from "vitest";
import { usePeopleUiStore } from "./uiStore";

function reset(): void {
  usePeopleUiStore.setState({ selectedIds: [], deleteRequest: null });
}

describe("people delete request", () => {
  beforeEach(reset);

  it("captures the selection when raised and is consumed once", () => {
    usePeopleUiStore.getState().setSelectedIds(["a", "b"]);
    usePeopleUiStore.getState().requestDelete();

    // A later selection change must not rewrite an already raised request.
    usePeopleUiStore.getState().setSelectedIds(["c"]);

    const request = usePeopleUiStore.getState().consumeDeleteRequest();
    expect(request?.ids).toEqual(["a", "b"]);
    expect(usePeopleUiStore.getState().deleteRequest).toBeNull();
    // Cancelling the confirmation does not resurrect it on a remount.
    expect(usePeopleUiStore.getState().consumeDeleteRequest()).toBeNull();
  });

  it("falls back to the focused contact when nothing is checked", () => {
    usePeopleUiStore.getState().requestDelete("focused");

    expect(usePeopleUiStore.getState().consumeDeleteRequest()?.ids).toEqual([
      "focused",
    ]);
  });

  it("queues nothing without a selection", () => {
    usePeopleUiStore.getState().requestDelete();
    expect(usePeopleUiStore.getState().deleteRequest).toBeNull();
  });
});
