// Fork-only probe: which LaunchServices / UTType calls JXA can reach on this macOS.
function t(name, f) { try { console.log(name + ': ' + f()); } catch (e) { console.log(name + ': THROWS ' + e); } }
t('import Foundation', () => { ObjC.import('Foundation'); return 'ok'; });
t('import CoreServices', () => { ObjC.import('CoreServices'); return 'ok'; });
t('import AppKit', () => { ObjC.import('AppKit'); return 'ok'; });
t('load UTI framework', () => $.NSBundle.bundleWithPath('/System/Library/Frameworks/UniformTypeIdentifiers.framework').load);
t('UTType class', () => typeof $.UTType);
t('UTType pdf', () => ObjC.unwrap($.UTType.typeWithIdentifier('com.adobe.pdf').identifier));
t('UTType ext svg', () => ObjC.unwrap($.UTType.typeWithFilenameExtension('svg').identifier));
t('UTType conforms', () => $.UTType.typeWithIdentifier('public.png').conformsToType($.UTType.typeWithIdentifier('public.image')));
t('LSCopy typeof', () => typeof $.LSCopyDefaultRoleHandlerForContentType);
t('LSSet typeof', () => typeof $.LSSetDefaultRoleHandlerForContentType);
t('LSCopy pdf', () => { const r = $.LSCopyDefaultRoleHandlerForContentType($('com.adobe.pdf'), 0xFFFFFFFF); return typeof r + ' ' + ObjC.unwrap(ObjC.castRefToObject(r)); });
t('LSCopy plain str', () => { const r = $.LSCopyDefaultRoleHandlerForContentType('public.plain-text', 0xFFFFFFFF); return ObjC.unwrap(ObjC.castRefToObject(r)); });
t('LSCopy none', () => { const r = $.LSCopyDefaultRoleHandlerForContentType('dyn.ah62d4rv4ge80000test', 0xFFFFFFFF); return typeof r + ' ' + r + ' ' + (r && r.isNil ? r.isNil() : 'n/a'); });
t('NSWorkspace url pdf', () => ObjC.unwrap($.NSWorkspace.sharedWorkspace.URLForApplicationToOpenContentType($.UTType.typeWithIdentifier('com.adobe.pdf')).path));
t('UTTypeCreatePreferred', () => ObjC.unwrap(ObjC.castRefToObject($.UTTypeCreatePreferredIdentifierForTag($('public.filename-extension'), $('svg'), $()))));
t('UTTypeConformsTo', () => $.UTTypeConformsTo($('public.png'), $('public.image')));
t('LSSet pdf -> Preview', () => $.LSSetDefaultRoleHandlerForContentType($('com.adobe.pdf'), 0xFFFFFFFF, $('com.apple.Preview')));
