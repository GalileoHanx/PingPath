// Router makers for the network doctor's SQM guidance
// (docs/decisions/011-home-network-doctor.md, "Router guidance"). The
// steps are in the translation files; the link is the maker's own support
// site. PingPath only shows these: it never changes a router.
"use strict";

const ROUTERS = [
  { id: "asus", name: () => t("router.asus.name"), steps: () => t("router.asus.steps"), url: "https://www.asus.com/support/" },
  { id: "tplink", name: () => t("router.tplink.name"), steps: () => t("router.tplink.steps"), url: "https://www.tp-link.com/support/" },
  { id: "netgear", name: () => t("router.netgear.name"), steps: () => t("router.netgear.steps"), url: "https://www.netgear.com/support/" },
  { id: "huawei", name: () => t("router.huawei.name"), steps: () => t("router.huawei.steps"), url: "https://consumer.huawei.com/en/support/" },
  { id: "zte", name: () => t("router.zte.name"), steps: () => t("router.zte.steps"), url: "https://www.zte.com.cn/global/" },
  { id: "dlink", name: () => t("router.dlink.name"), steps: () => t("router.dlink.steps"), url: "https://support.dlink.com/" },
  { id: "linksys", name: () => t("router.linksys.name"), steps: () => t("router.linksys.steps"), url: "https://support.linksys.com/" },
  { id: "mikrotik", name: () => t("router.mikrotik.name"), steps: () => t("router.mikrotik.steps"), url: "https://help.mikrotik.com/docs/" },
  { id: "ubiquiti", name: () => t("router.ubiquiti.name"), steps: () => t("router.ubiquiti.steps"), url: "https://help.ui.com/" },
  { id: "openwrt", name: () => t("router.openwrt.name"), steps: () => t("router.openwrt.steps"), url: "https://openwrt.org/docs/guide-user/network/traffic-shaping/sqm" },
  { id: "other", name: () => t("router.other.name"), steps: () => t("router.other.steps"), url: null },
];
