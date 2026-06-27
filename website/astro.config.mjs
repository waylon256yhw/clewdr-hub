// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// 部署目标：GitHub Pages 项目站点 https://waylon256yhw.github.io/clewdr-hub/
// 自定义域名时把 site 改成域名、base 改回 '/' 即可。
// https://astro.build/config
export default defineConfig({
	site: 'https://waylon256yhw.github.io',
	base: '/clewdr-hub/',
	integrations: [
		starlight({
			title: 'clewdr-hub',
			description: '基于 clewdr 的多用户 Claude 共享网关',
			logo: {
				src: './src/assets/logo.svg',
				alt: 'clewdr-hub',
			},
			favicon: '/favicon.svg',
			customCss: ['./src/styles/heritage.css'],
			// 中文为主；将来加英文只需补 locales.en + src/content/docs/en/。
			defaultLocale: 'root',
			locales: {
				root: { label: '简体中文', lang: 'zh-CN' },
			},
			social: [
				{
					icon: 'github',
					label: 'GitHub',
					href: 'https://github.com/waylon256yhw/clewdr-hub',
				},
			],
			editLink: {
				baseUrl:
					'https://github.com/waylon256yhw/clewdr-hub/edit/master/website/',
			},
			lastUpdated: true,
			sidebar: [
				{ label: '入门', items: [{ autogenerate: { directory: 'start' } }] },
				{ label: '使用指南', items: [{ autogenerate: { directory: 'guides' } }] },
				{ label: '参考', items: [{ autogenerate: { directory: 'reference' } }] },
				{ label: '开发', items: [{ autogenerate: { directory: 'dev' } }] },
			],
		}),
	],
});
