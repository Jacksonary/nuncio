/**
 * Generic chips input (recipients, attendees) with async suggestions.
 * Enter / comma / semicolon commits, Backspace removes, paste splits addresses.
 */
import {
  useEffect,
  useId,
  useRef,
  useState,
  type ClipboardEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { Dismiss16Regular } from "@fluentui/react-icons";
import type { Address } from "@/ipc/types";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { formatAddress, isValidEmail, parseAddressInput } from "@/lib/format";
import { Avatar } from "./Avatar";
import { Popover } from "./Popover";
import styles from "./TokenInput.module.css";

export interface TokenInputProps {
  value: Address[];
  onChange: (value: Address[]) => void;
  suggestions?: (query: string) => Address[] | Promise<Address[]>;
  placeholder?: string;
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  disabled?: boolean;
  autoFocus?: boolean;
  max?: number;
  className?: string;
  id?: string;
  showAvatars?: boolean;
  ariaLabel?: string;
  renderToken?: (address: Address) => ReactNode;
}

function sameEmail(a: string, b: string): boolean {
  return a.trim().toLowerCase() === b.trim().toLowerCase();
}

export function TokenInput({
  value,
  onChange,
  suggestions,
  placeholder,
  label,
  hint,
  error,
  disabled = false,
  autoFocus = false,
  max,
  className,
  id: idProp,
  showAvatars = true,
  ariaLabel,
  renderToken,
}: TokenInputProps) {
  const t = useT();
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const listId = `${id}-suggestions`;
  const inputRef = useRef<HTMLInputElement | null>(null);
  const wrapperRef = useRef<HTMLDivElement | null>(null);
  const blurTimer = useRef<number | null>(null);
  const [text, setText] = useState("");
  const [open, setOpen] = useState(false);
  const [items, setItems] = useState<Address[]>([]);
  const [highlighted, setHighlighted] = useState(-1);
  const [loading, setLoading] = useState(false);
  const [invalid, setInvalid] = useState(false);

  const atLimit = max !== undefined && value.length >= max;

  useEffect(() => {
    if (autoFocus) inputRef.current?.focus();
  }, [autoFocus]);

  useEffect(() => {
    if (!open || !suggestions) {
      setItems([]);
      return;
    }
    let cancelled = false;
    setLoading(true);
    const timer = window.setTimeout(() => {
      Promise.resolve(suggestions(text))
        .then((result) => {
          if (cancelled) return;
          setItems(
            result.filter(
              (address) => !value.some((existing) => sameEmail(existing.email, address.email)),
            ),
          );
          setHighlighted(-1);
        })
        .catch(() => {
          if (!cancelled) setItems([]);
        })
        .finally(() => {
          if (!cancelled) setLoading(false);
        });
    }, 160);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text, open, suggestions, value.length]);

  useEffect(() => {
    return () => {
      if (blurTimer.current !== null) window.clearTimeout(blurTimer.current);
    };
  }, []);

  const add = (addresses: Address[]) => {
    if (addresses.length === 0) return;
    const next = [...value];
    for (const address of addresses) {
      const email = address.email.trim();
      if (!email || !isValidEmail(email)) continue;
      if (next.some((existing) => sameEmail(existing.email, email))) continue;
      if (max !== undefined && next.length >= max) break;
      next.push({ name: address.name?.trim() || null, email });
    }
    if (next.length !== value.length) onChange(next);
  };

  const commitText = (): boolean => {
    const trimmed = text.trim();
    if (!trimmed) return false;
    const parsed = parseAddressInput(trimmed);
    const valid = parsed.filter((address) => isValidEmail(address.email));
    if (valid.length === 0) {
      setInvalid(true);
      return false;
    }
    add(valid);
    setText("");
    setInvalid(false);
    setHighlighted(-1);
    return true;
  };

  const choose = (address: Address) => {
    add([address]);
    setText("");
    setHighlighted(-1);
    inputRef.current?.focus();
  };

  const removeAt = (index: number) => {
    const next = value.filter((_, current) => current !== index);
    onChange(next);
    inputRef.current?.focus();
  };

  const closeSuggestions = () => {
    setOpen(false);
    setHighlighted(-1);
  };

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        if (!open) {
          setOpen(true);
          return;
        }
        if (items.length > 0) {
          setHighlighted((current) => (current + 1) % items.length);
        }
        break;
      case "ArrowUp":
        event.preventDefault();
        if (items.length > 0) {
          setHighlighted((current) => (current - 1 + items.length) % items.length);
        }
        break;
      case "Enter":
        event.preventDefault();
        if (open && highlighted >= 0 && items[highlighted]) {
          choose(items[highlighted]);
        } else {
          commitText();
        }
        break;
      case "Escape":
        if (open) {
          event.preventDefault();
          closeSuggestions();
        }
        break;
      case ",":
      case ";":
        event.preventDefault();
        commitText();
        break;
      case "Backspace":
        if (!text && value.length > 0) {
          event.preventDefault();
          removeAt(value.length - 1);
        }
        break;
      case "Tab":
        if (text.trim()) commitText();
        closeSuggestions();
        break;
      default:
        break;
    }
  };

  const handlePaste = (event: ClipboardEvent<HTMLInputElement>) => {
    const pasted = event.clipboardData.getData("text");
    if (!pasted) return;
    if (/[,;\n]/.test(pasted)) {
      event.preventDefault();
      const parsed = parseAddressInput(pasted).filter((address) =>
        isValidEmail(address.email),
      );
      if (parsed.length > 0) {
        add(parsed);
        setText("");
        setInvalid(false);
      }
    }
  };

  return (
    <div className={cn(styles.field, className)}>
      {label ? (
        <label htmlFor={id} className={styles.label}>
          {label}
        </label>
      ) : null}
      <div
        ref={wrapperRef}
        className={cn(
          styles.tokenField,
          (error || invalid) && styles.invalid,
          disabled && styles.disabled,
        )}
        onClick={() => inputRef.current?.focus()}
      >
        {value.map((address, index) => (
          <span className={styles.token} key={`${address.email}-${index}`}>
            {renderToken ? (
              renderToken(address)
            ) : (
              <>
                {showAvatars ? (
                  <Avatar name={address.name} email={address.email} size={18} />
                ) : null}
                <span className={styles.tokenLabel}>{formatAddress(address)}</span>
              </>
            )}
            <button
              type="button"
              className={styles.tokenRemove}
              aria-label={t("common.removeItem", { item: formatAddress(address) })}
              onClick={(event) => {
                event.stopPropagation();
                removeAt(index);
              }}
            >
              <Dismiss16Regular />
            </button>
          </span>
        ))}
        <input
          ref={inputRef}
          id={id}
          className={styles.input}
          type="text"
          role="combobox"
          aria-expanded={open}
          aria-controls={open ? listId : undefined}
          aria-autocomplete="list"
          aria-activedescendant={
            open && highlighted >= 0 ? `${listId}-${highlighted}` : undefined
          }
          aria-label={ariaLabel ?? (typeof label === "string" ? label : undefined)}
          aria-invalid={error || invalid ? true : undefined}
          value={text}
          placeholder={value.length === 0 ? placeholder : undefined}
          disabled={disabled || atLimit}
          autoComplete="off"
          spellCheck={false}
          onChange={(event) => {
            setText(event.target.value);
            setInvalid(false);
            setOpen(true);
          }}
          onFocus={() => {
            if (blurTimer.current !== null) window.clearTimeout(blurTimer.current);
            setOpen(true);
          }}
          onBlur={() => {
            blurTimer.current = window.setTimeout(() => {
              if (!wrapperRef.current?.contains(document.activeElement)) {
                if (text.trim()) commitText();
                closeSuggestions();
              }
            }, 120);
          }}
          onKeyDown={handleKeyDown}
          onPaste={handlePaste}
        />
      </div>

      <Popover
        open={open && Boolean(suggestions) && !atLimit}
        onClose={closeSuggestions}
        anchor={wrapperRef.current}
        placement="bottom-start"
        matchAnchorWidth
        role="listbox"
        id={listId}
        ariaLabel={t("common.suggestions")}
        className={styles.suggestions}
        closeOnEscape={false}
      >
        {items.length === 0 ? (
          <div className={styles.empty}>
            {loading ? t("common.loading") : t("common.noSuggestions")}
          </div>
        ) : (
          items.map((address, index) => (
            <button
              key={`${address.email}-${index}`}
              id={`${listId}-${index}`}
              type="button"
              role="option"
              aria-selected={highlighted === index}
              className={cn(styles.option, highlighted === index && styles.highlighted)}
              onMouseDown={(event) => event.preventDefault()}
              onMouseEnter={() => setHighlighted(index)}
              onClick={() => choose(address)}
            >
              <Avatar name={address.name} email={address.email} size={28} />
              <span className={styles.optionLabel}>
                {address.name ? (
                  <span className={styles.optionName}>{address.name}</span>
                ) : null}
                <span className={styles.optionEmail}>{address.email}</span>
              </span>
            </button>
          ))
        )}
      </Popover>

      {error || invalid || hint ? (
        <div
          className={cn(styles.message, (error || invalid) && styles.error)}
          role={error || invalid ? "alert" : undefined}
        >
          {error || (invalid ? t("common.invalidEmail") : hint)}
        </div>
      ) : null}
    </div>
  );
}
