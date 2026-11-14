import {
  forwardRef,
  type ButtonHTMLAttributes,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { Button, type ButtonAppearance, type ButtonSize } from "./Button";
import styles from "./ToggleButton.module.css";

export interface ToggleButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onChange" | "onClick"> {
  pressed: boolean;
  onPressedChange: (pressed: boolean) => void;
  appearance?: ButtonAppearance;
  size?: ButtonSize;
  icon?: ReactNode;
  children?: ReactNode;
}

/** Button with `aria-pressed` state (toolbar formatting, filters, …). */
export const ToggleButton = forwardRef<HTMLButtonElement, ToggleButtonProps>(
  function ToggleButton(
    {
      pressed,
      onPressedChange,
      appearance = "transparent",
      icon,
      children,
      className,
      disabled,
      ...rest
    },
    ref,
  ) {
    return (
      <Button
        ref={ref}
        appearance={appearance}
        icon={icon}
        aria-pressed={pressed}
        className={cn(
          pressed && (appearance === "primary" ? styles.pressedPrimary : styles.pressed),
          className,
        )}
        disabled={disabled}
        onClick={() => onPressedChange(!pressed)}
        {...rest}
      >
        {children}
      </Button>
    );
  },
);

ToggleButton.displayName = "ToggleButton";
