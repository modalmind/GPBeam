import { mount } from "svelte";
import Popover from "./Popover.svelte";

const target = document.getElementById("app");
if (!target) throw new Error("popover mount target #app not found");

const app = mount(Popover, { target });

export default app;
