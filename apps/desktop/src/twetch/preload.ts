import { contextBridge, ipcRenderer } from "electron";

contextBridge.exposeInMainWorld("twetchWallet", {
  open: () => ipcRenderer.invoke("twetch:wallet-open"),
  status: () => ipcRenderer.invoke("twetch:wallet-status"),
});
