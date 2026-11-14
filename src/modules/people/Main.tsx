/**
 * People main area: resizable list + detail panes and the create/edit dialog.
 * The list/detail split is persisted by the shared Splitter component.
 */
import { Splitter } from "@/components";
import { useT } from "@/lib/i18n";
import { ContactList } from "./ContactList";
import { ContactDetail } from "./ContactDetail";
import { ContactEditor } from "./ContactEditor";
import styles from "./People.module.css";

export function PeopleMain() {
  const t = useT();
  return (
    <div className={styles.peopleMain}>
      <Splitter
        storageKey="people.list"
        defaultSize={360}
        min={280}
        max={640}
        primary={<ContactList />}
        secondary={<ContactDetail />}
        handleLabel={t("common.dragToResize")}
        primaryClassName={styles.listColumn}
        secondaryClassName={styles.detailColumn}
        resetOnDoubleClick
      />
      <ContactEditor />
    </div>
  );
}

export default PeopleMain;
