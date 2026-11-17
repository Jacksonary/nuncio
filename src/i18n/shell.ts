/**
 * Shell dictionary (owned by F-shell): title bar, navigation, first-run,
 * sync status, reminder toasts and file-import prompts.
 */
export const shell = {
  "zh-CN": {
    "shell.appName": "Nuncio",
    "shell.mail": "邮件",
    "shell.calendar": "日历",
    "shell.people": "联系人",
    "shell.todo": "待办",
    "shell.settings": "设置",

    "shell.newMail": "新邮件",
    "shell.newEvent": "新建事件",
    "shell.newContact": "新建联系人",
    "shell.newTask": "新建任务",

    "shell.toggleRail": "折叠或展开导航",
    "shell.moduleSwitcher": "模块切换",

    "shell.syncStatus": "同步状态",
    "shell.syncProgress": "正在同步 {done}/{total}",
    "shell.syncPending": "正在同步…",
    "shell.synced": "已同步",
    "shell.neverSynced": "尚未同步",
    "shell.syncFailed": "同步出错",
    "shell.needsReauth": "需要重新登录",
    "shell.syncOffline": "离线，稍后自动重试",
    "shell.offlineBanner": "当前离线，操作将在恢复连接后自动同步。",
    "shell.offlineRetry": "立即检查",
    "shell.syncStarted": "已开始同步",
    "shell.syncFailedToast": "同步失败：{message}",

    "shell.firstRun.title": "欢迎使用 Nuncio",
    "shell.firstRun.subtitle":
      "轻量的邮件、日历、联系人与待办，一个应用全部搞定。",
    "shell.firstRun.addAccount": "添加账户",
    "shell.firstRun.skip": "仅使用本地日历、联系人和待办",
    "shell.firstRun.featureLocal": "数据只在本机与服务商之间流动，没有账号与遥测",
    "shell.firstRun.featureUnified": "合并收件箱、离线操作队列与本地搜索",
    "shell.firstRun.featureModules": "邮件、日历、联系人、待办共用同一套界面",
    "shell.firstRun.privacy": "账户密码保存在系统钥匙串中，不会写入数据库或日志。",

    "shell.addAccountDialog.title": "添加账户",
    "shell.addAccountDialog.hint":
      "账户添加向导由设置模块提供；你也可以先在设置中填写服务器信息。",

    "shell.reminder.title": "提醒",
    "shell.reminder.snooze": "10 分钟后提醒",
    "shell.reminder.snoozeHour": "1 小时后提醒",
    "shell.reminder.allDay": "全天",
    "shell.reminder.open": "打开",
    "shell.reminder.dismiss": "关闭提醒",
    "shell.reminder.eventBody": "{time} · {location}",
    "shell.reminder.taskBody": "待办提醒",

    "shell.opFailed.title": "操作失败",
    "shell.opFailed.body": "{description}：{error}",

    "shell.importConfirm": "导入",
    "shell.importIcs.title": "导入日历事件",
    "shell.importIcs.choose": "选择要导入到的日历",
    "shell.importIcs.noCalendarsTitle": "先创建一个日历",
    "shell.importIcs.noCalendars": "没有可用的日历，请先创建一个日历。",
    "shell.importIcs.done": "已导入 {count} 个事件",
    "shell.importIcs.failed": "导入日历事件失败：{message}",

    "shell.importVcf.title": "导入联系人",
    "shell.importVcf.choose": "选择要导入到的通讯录",
    "shell.importVcf.noAddressBooksTitle": "先创建一个通讯录",
    "shell.importVcf.noAddressBooks": "没有可用的通讯录。",
    "shell.importVcf.done": "已导入 {count} 个联系人",
    "shell.importVcf.failed": "导入联系人失败：{message}",

    "shell.webcal.subscribed": "已订阅日历“{name}”",
    "shell.webcal.failed": "订阅日历失败：{message}",

    "shell.openFileFailed": "无法打开文件：{message}",
    "shell.composeFailed": "无法打开撰写窗口：{message}",

    "shell.placeholderHint": "“{module}”模块正在接入，界面框架已经就绪。",
    "shell.settingsPlaceholder": "设置面板由设置模块提供。",
    "shell.accountsPlaceholder": "账户管理由设置模块提供。",

    "shell.window.message": "邮件",
    "shell.window.compose": "新邮件",
    "shell.window.close": "关闭窗口",

    "shell.shortcuts.modules": "切换模块",
    "shell.shortcuts.settings": "打开设置",
    "shell.shortcuts.sync": "同步全部账户",
  },
  "en-US": {
    "shell.appName": "Nuncio",
    "shell.mail": "Mail",
    "shell.calendar": "Calendar",
    "shell.people": "People",
    "shell.todo": "To Do",
    "shell.settings": "Settings",

    "shell.newMail": "New mail",
    "shell.newEvent": "New event",
    "shell.newContact": "New contact",
    "shell.newTask": "New task",

    "shell.toggleRail": "Collapse or expand navigation",
    "shell.moduleSwitcher": "Modules",

    "shell.syncStatus": "Sync status",
    "shell.syncProgress": "Syncing {done}/{total}",
    "shell.syncPending": "Syncing…",
    "shell.synced": "Synced",
    "shell.neverSynced": "Not synced yet",
    "shell.syncFailed": "Sync error",
    "shell.needsReauth": "Sign-in required",
    "shell.syncOffline": "Offline — will retry automatically",
    "shell.offlineBanner": "You are offline. Changes will sync when the connection returns.",
    "shell.offlineRetry": "Check now",
    "shell.syncStarted": "Sync started",
    "shell.syncFailedToast": "Sync failed: {message}",

    "shell.firstRun.title": "Welcome to Nuncio",
    "shell.firstRun.subtitle":
      "Mail, calendar, contacts and to-dos in one lightweight app.",
    "shell.firstRun.addAccount": "Add account",
    "shell.firstRun.skip": "Use local calendar, contacts and to-dos only",
    "shell.firstRun.featureLocal":
      "Your data only travels between this device and your providers — no accounts, no telemetry",
    "shell.firstRun.featureUnified":
      "Unified inbox, offline operation queue and local search",
    "shell.firstRun.featureModules":
      "Mail, calendar, contacts and to-dos share one consistent interface",
    "shell.firstRun.privacy":
      "Passwords live in the OS keychain and are never written to the database or logs.",

    "shell.addAccountDialog.title": "Add account",
    "shell.addAccountDialog.hint":
      "The account wizard is provided by the settings module; you can also enter server details in Settings.",

    "shell.reminder.title": "Reminder",
    "shell.reminder.snooze": "Snooze 10 minutes",
    "shell.reminder.snoozeHour": "Snooze 1 hour",
    "shell.reminder.allDay": "All day",
    "shell.reminder.open": "Open",
    "shell.reminder.dismiss": "Dismiss reminder",
    "shell.reminder.eventBody": "{time} · {location}",
    "shell.reminder.taskBody": "Task reminder",

    "shell.opFailed.title": "Operation failed",
    "shell.opFailed.body": "{description}: {error}",

    "shell.importConfirm": "Import",
    "shell.importIcs.title": "Import calendar events",
    "shell.importIcs.choose": "Choose the calendar to import into",
    "shell.importIcs.noCalendarsTitle": "Create a calendar first",
    "shell.importIcs.noCalendars": "There are no calendars yet. Create one first.",
    "shell.importIcs.done": "Imported {count} events",
    "shell.importIcs.failed": "Could not import calendar events: {message}",

    "shell.importVcf.title": "Import contacts",
    "shell.importVcf.choose": "Choose the address book to import into",
    "shell.importVcf.noAddressBooksTitle": "Create an address book first",
    "shell.importVcf.noAddressBooks": "There are no address books.",
    "shell.importVcf.done": "Imported {count} contacts",
    "shell.importVcf.failed": "Could not import contacts: {message}",

    "shell.webcal.subscribed": "Subscribed to “{name}”",
    "shell.webcal.failed": "Could not subscribe to the calendar: {message}",

    "shell.openFileFailed": "Could not open the file: {message}",
    "shell.composeFailed": "Could not open the compose window: {message}",

    "shell.placeholderHint": "The “{module}” module is being connected; the shell around it is ready.",
    "shell.settingsPlaceholder": "The settings panel is provided by the settings module.",
    "shell.accountsPlaceholder": "Account management is provided by the settings module.",

    "shell.window.message": "Message",
    "shell.window.compose": "New message",
    "shell.window.close": "Close window",

    "shell.shortcuts.modules": "Switch modules",
    "shell.shortcuts.settings": "Open settings",
    "shell.shortcuts.sync": "Sync all accounts",
  },
} as const;
