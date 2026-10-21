// One entry per page in the decisions doc ("前端页面").
import type { Component } from 'vue'

export interface PageDef {
  path: string
  title: string
  component: () => Promise<{ default: Component }>
}

export const pages: PageDef[] = [
  { path: '/tasks', title: '任务', component: () => import('./pages/TasksPage.vue') },
  { path: '/new', title: '新建转码', component: () => import('./pages/NewTaskPage.vue') },
  { path: '/monitor', title: '监控', component: () => import('./pages/MonitorPage.vue') },
  { path: '/profiles', title: '配置方案', component: () => import('./pages/ProfilesPage.vue') },
  { path: '/recommend', title: '样片推荐', component: () => import('./pages/RecommendPage.vue') },
  { path: '/settings', title: '设置与诊断', component: () => import('./pages/SettingsPage.vue') },
]
