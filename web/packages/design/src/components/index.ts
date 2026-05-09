// @forge/design — button primitive surface (F-450).
//
// Re-exports the four primitives shared by the app and a single CSS file
// (`./forge-button.css`) consumers must import once at the app shell so the
// primitives' baseline styling is on the page.

export { Button, type ButtonProps, type ButtonVariant, type ButtonSize } from './Button';
export {
  IconButton,
  type IconButtonProps,
  type IconButtonVariant,
} from './IconButton';
export {
  Tab,
  Tabs,
  type TabProps,
  type TabsProps,
  type TabsVariant,
} from './Tab';
export { MenuItem, type MenuItemProps, type MenuItemVariant } from './MenuItem';
export {
  Skeleton,
  type SkeletonProps,
  type SkeletonVariant,
} from './Skeleton';
