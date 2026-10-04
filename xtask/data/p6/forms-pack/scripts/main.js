// cargo xtask p6 forms scenario: on "/scriptevent p6:<kind>" every player gets the form of that kind, and
// the answer goes to the content log. Anchor's harness sends the same forms with its form API
// (xtask/src/p6/game/console.rs, `p6_form`); keep the two the same.
import { system, world } from "@minecraft/server";
import { ActionFormData, MessageFormData, ModalFormData } from "@minecraft/server-ui";

// Calls form[name](...args) when this API version has the method; the content log notes when it does not.
function optional(form, name, ...args) {
  if (typeof form[name] === "function") {
    return form[name](...args);
  }
  console.warn(`p6 forms: no ${name} in this API version`);
  return form;
}

const forms = {
  modal: () => new MessageFormData().title("P6 modal").body("Modal body").button1("Yes").button2("No"),
  menu: () => {
    let f = new ActionFormData().title("P6 menu").body("Menu body").button("First");
    f = optional(f, "header", "Menu header");
    f = optional(f, "label", "Menu label");
    f = optional(f, "divider");
    return f.button("Second", "textures/items/apple");
  },
  custom: () => {
    let f = new ModalFormData().title("P6 custom");
    f = optional(f, "header", "A header");
    f = optional(f, "label", "A label");
    f = optional(f, "divider");
    return f
      .textField("Name", "placeholder", { defaultValue: "default" })
      .toggle("Toggle", { defaultValue: false })
      .slider("Slider", 0, 10, { valueStep: 1, defaultValue: 3 })
      .dropdown("Dropdown", ["a", "b", "c"], { defaultValueIndex: 0 });
  },
  closed: () => new MessageFormData().title("P6 closed").body("Closed body").button1("Yes").button2("No"),
};

system.afterEvents.scriptEventReceive.subscribe((event) => {
  const kind = event.id.startsWith("p6:") ? event.id.slice(3) : "";
  const make = forms[kind];
  if (make === undefined) {
    return;
  }
  for (const player of world.getAllPlayers()) {
    make()
      .show(player)
      .then((response) => console.warn(`p6 forms: ${kind} answered ${JSON.stringify(response)}`))
      .catch((error) => console.warn(`p6 forms: ${kind} failed ${error}`));
  }
});
