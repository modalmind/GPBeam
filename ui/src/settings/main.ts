import { mount } from "svelte";
import Settings from "./Settings.svelte";

const target = document.getElementById("app");
if (!target) throw new Error("settings mount target #app not found");

const app = mount(Settings, { target });

export default app;
