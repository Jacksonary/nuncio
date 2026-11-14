/**
 * Thin wrapper around @tanstack/react-virtual. Handles the scroll container,
 * absolute row positioning and an optional end-reached callback for paging.
 */
import { useEffect, useRef, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "@/lib/cn";
import styles from "./VirtualList.module.css";

export interface VirtualListProps<T> {
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
}

export function VirtualList<T>({
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
              role="listitem"
            >
              {renderItem(item, virtualRow.index)}
            </div>
          );
        })}
      </div>
    </div>
  );
}
