// Router makers for the network doctor's SQM guidance
// (docs/decisions/011-home-network-doctor.md, "Router guidance"), given
// the page's t(): the steps are in the translation files, and the window
// opens each maker's support site (src/main.rs, ROUTER_GUIDES; "other"
// has none). PingPath only shows these: it never changes a router.
"use strict";

window.PingPathRouters = function (t) {
  return [
    { id: "asus", name: t("router.asus.name"), steps: t("router.asus.steps"), link: true },
    { id: "tplink", name: t("router.tplink.name"), steps: t("router.tplink.steps"), link: true },
    { id: "netgear", name: t("router.netgear.name"), steps: t("router.netgear.steps"), link: true },
    { id: "huawei", name: t("router.huawei.name"), steps: t("router.huawei.steps"), link: true },
    { id: "zte", name: t("router.zte.name"), steps: t("router.zte.steps"), link: true },
    { id: "dlink", name: t("router.dlink.name"), steps: t("router.dlink.steps"), link: true },
    { id: "linksys", name: t("router.linksys.name"), steps: t("router.linksys.steps"), link: true },
    { id: "mikrotik", name: t("router.mikrotik.name"), steps: t("router.mikrotik.steps"), link: true },
    { id: "ubiquiti", name: t("router.ubiquiti.name"), steps: t("router.ubiquiti.steps"), link: true },
    { id: "openwrt", name: t("router.openwrt.name"), steps: t("router.openwrt.steps"), link: true },
    { id: "other", name: t("router.other.name"), steps: t("router.other.steps"), link: false },
  ];
};
