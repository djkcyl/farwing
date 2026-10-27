// All messages, by language: one file per area, each with both languages
// side by side. The Chinese object is the type; English must match it.
import type { Lang } from '../lang';
import * as account from './account';
import * as admin from './admin';
import * as auth from './auth';
import * as common from './common';
import * as devices from './devices';
import * as format from './format';
import * as nav from './nav';
import * as notices from './notices';
import * as records from './records';
import * as setup from './setup';
import * as viewer from './viewer';

const zh = {
  common: common.zh,
  format: format.zh,
  nav: nav.zh,
  auth: auth.zh,
  devices: devices.zh,
  account: account.zh,
  admin: admin.zh,
  setup: setup.zh,
  viewer: viewer.zh,
  records: records.zh,
  notices: notices.zh,
};

export type Messages = typeof zh;

const en: Messages = {
  common: common.en,
  format: format.en,
  nav: nav.en,
  auth: auth.en,
  devices: devices.en,
  account: account.en,
  admin: admin.en,
  setup: setup.en,
  viewer: viewer.en,
  records: records.en,
  notices: notices.en,
};

export const messages: Record<Lang, Messages> = { 'zh-CN': zh, en };
