/**
 * Module registry. The shell reads modules from here; agents only edit the
 * module folders, not this file.
 */
import type { AppModule } from "@/ipc/types";
import type { ModuleDefinition } from "@/app/moduleTypes";
import mail from "@/modules/mail";
import calendar from "@/modules/calendar";
import people from "@/modules/people";
import todo from "@/modules/todo";

export const moduleOrder: AppModule[] = ["mail", "calendar", "people", "todo"];

export const modules: Record<AppModule, ModuleDefinition> = {
  mail,
  calendar,
  people,
  todo,
};

/** i18n key for each module's primary "new" action. */
export const newActionLabelKeys: Record<AppModule, string> = {
  mail: "shell.newMail",
  calendar: "shell.newEvent",
  people: "shell.newContact",
  todo: "shell.newTask",
};
