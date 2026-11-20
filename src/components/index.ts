/**
 * Public component barrel for module agents.
 * Import primitives from "@/components" rather than deep paths.
 */
export { Button, DangerButton } from "./Button";
export type { ButtonProps, ButtonAppearance, ButtonSize } from "./Button";

export { IconButton } from "./IconButton";
export type { IconButtonProps } from "./IconButton";

export { ToggleButton } from "./ToggleButton";
export type { ToggleButtonProps } from "./ToggleButton";

export { WindowControls } from "./WindowControls";
export type { WindowControlsProps } from "./WindowControls";

export { SplitButton } from "./SplitButton";
export type { SplitButtonProps } from "./SplitButton";

export { Input, SearchBox, TextArea } from "./Input";
export type { InputProps, SearchBoxProps, TextAreaProps, FieldSize } from "./Input";

export { Select } from "./Select";
export type { SelectProps, SelectOption } from "./Select";

export { Checkbox, Radio, RadioGroup, Switch } from "./Toggle";
export type { CheckboxProps, RadioProps, RadioGroupProps, RadioOption, SwitchProps } from "./Toggle";

export { Slider } from "./Slider";
export type { SliderProps } from "./Slider";

export { DatePicker, TimePicker } from "./DateTime";
export type { DatePickerProps, TimePickerProps } from "./DateTime";

export { ColorPicker, COLOR_PRESETS } from "./ColorPicker";
export type { ColorPickerProps } from "./ColorPicker";

export { Menu, MenuButton, ContextMenu, isMenuItem } from "./Menu";
export type {
  MenuProps,
  MenuButtonProps,
  ContextMenuProps,
  MenuEntry,
  MenuItemDef,
  MenuSeparatorDef,
  MenuHeaderDef,
} from "./Menu";

export { Popover } from "./Popover";
export type { PopoverProps, PopoverPlacement } from "./Popover";

export { Tooltip, RichTooltip } from "./Tooltip";
export type { TooltipProps, RichTooltipProps } from "./Tooltip";

export { Dialog, ConfirmDialog, ConfirmHost, confirm } from "./Dialog";
export type { DialogProps, ConfirmOptions, ConfirmDialogProps } from "./Dialog";

export { SidePanel } from "./SidePanel";
export type { SidePanelProps } from "./SidePanel";

export { ToastProvider, ToastViewport, useToast, toast } from "./Toast";
export type {
  ToastApi,
  ToastProviderProps,
  ToastOptions,
  ToastItem,
  ToastAction,
  ToastType,
} from "./Toast";

export { Spinner, ProgressBar } from "./Progress";
export type { SpinnerProps, ProgressBarProps } from "./Progress";

export { Avatar, avatarColor } from "./Avatar";
export type { AvatarProps } from "./Avatar";

export { Badge, CountBadge, DotBadge } from "./Badge";
export type { BadgeProps, CountBadgeProps, DotBadgeProps, BadgeAppearance } from "./Badge";

export { EmptyState } from "./EmptyState";
export type { EmptyStateProps } from "./EmptyState";

export { Splitter } from "./Splitter";
export type { SplitterProps } from "./Splitter";

export { Tabs } from "./Tabs";
export type { TabsProps, TabItem } from "./Tabs";

export { CommandBar, Ribbon } from "./CommandBar";
export type { CommandBarProps, CommandBarItem, RibbonProps } from "./CommandBar";

export { Card } from "./Card";
export type { CardProps } from "./Card";

export { ListItem } from "./ListItem";
export type { ListItemProps, ListDensity } from "./ListItem";

export { Kbd } from "./Kbd";
export type { KbdProps } from "./Kbd";

export { VirtualList } from "./VirtualList";
export type { VirtualListHandle, VirtualListProps } from "./VirtualList";

export { TokenInput } from "./TokenInput";
export type { TokenInputProps } from "./TokenInput";

export {
  ModuleSyncStatus,
  ModuleSyncDot,
  presentationFor,
} from "./ModuleSyncStatus";
export type {
  ModuleSyncStatusProps,
  ModuleSyncDotProps,
  SyncPresentation,
} from "./ModuleSyncStatus";
