/**
 * Thin wrapper around @tanstack/react-virtual. Handles the scroll container,
 * absolute row positioning and an optional end-reached callback for paging.
 */
import { useEffect, useImperativeHandle, useRef, type ReactNode, type Ref } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "@/lib/cn";
import styles from "./VirtualList.module.css";

/** Imperative API; uses the virtualizer's measured row positions. */
export interface VirtualListHandle {
  scrollToIndex: (index: number, options?: { align?: "start" | "center" | "end" | "auto" }) => void;
}

export interface VirtualListProps<T> {
  handleRef?: Ref<VirtualListHandle>;
  items: T[];
  renderItem: (item: T, index: number) => ReactNode;
  /** Estimated row height; rows are measured after mount. */
  estimateSize?: number;
  overscan?: number;
  getKey?: (item: T, index: number) => string | number;
  className?: string;
  itemClassName?: string;
  emptyState?: ReactNode;
  /** Called when the last row becomes visible (for cursor paging). */
  onEndReached?: () => void;
  ariaLabel?: string;
  role?: string;
  /**
   * Role for the per-row wrapper. Defaults to "listitem"; a listbox owner must
   * pass "option" (or "presentation" when its children carry the option role)
   * because a listbox may not own listitems.
   */
  itemRole?: string;
}

export function VirtualList<T>({
  handleRef,
  items,
  renderItem,
  estimateSize = 56,
  overscan = 10,
  getKey,
  className,
  itemClassName,
  emptyState,
  onEndReached,
  ariaLabel,
  role = "list",
  itemRole = "listitem",
}: VirtualListProps<T>) {
  const parentRef = useRef<HTMLDivElement | null>(null);

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => estimateSize,
    overscan,
    getItemKey: (index) => {
      const item = items[index];
      return getKey && item !== undefined ? getKey(item, index) : index;
    },
  });

  useImperativeHandle(
    handleRef,
    () => ({
      scrollToIndex: (index, options) => {
        if (index < 0 || index >= items.length) return;
        virtualizer.scrollToIndex(index, { align: options?.align ?? "auto" });
      },
    }),
    [virtualizer, items.length],
  );

  const virtualItems = virtualizer.getVirtualItems();
  const lastIndex = virtualItems.length > 0 ? virtualItems[virtualItems.length - 1].index : -1;
  const end = items.length - 1;

  useEffect(() => {
    if (!onEndReached) return;
    if (items.length === 0) return;
    if (lastIndex >= end) onEndReached();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lastIndex, end, items.length]);

  if (items.length === 0 && emptyState !== undefined) {
    return <div className={cn(styles.container, className)}>{emptyState}</div>;
  }

  return (
    <div
      ref={parentRef}
      className={cn(styles.container, className)}
      role={role}
      aria-label={ariaLabel}
    >
      <div className={styles.inner} style={{ height: virtualizer.getTotalSize() }}>
        {virtualItems.map((virtualRow) => {
          const item = items[virtualRow.index];
          if (item === undefined) return null;
          return (
            <div
              key={virtualRow.key}
              ref={virtualizer.measureElement}
              data-index={virtualRow.index}
              className={cn(styles.row, itemClassName)}
              style={{ transform: `translateY(${virtualRow.start}px)` }}
              role={itemRole}
            >
              {renderItem(item, virtualRow.index)}
            </div>
          );
        })}
      </div>
    </div>
  );
}
