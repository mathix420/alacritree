//! The app derives `Delegate` for a trait defined in another crate.  This is
//! what enum_dispatch cannot do, and the reason the integration crates use
//! ambassador; if it stops compiling, every integration enum breaks with it.

use std::path::Path;

use alacritree_checkout_hooks::fake::{Event, FakeHook};
use alacritree_checkout_hooks::{
    Checkout, CheckoutHook, CheckoutHooks, ambassador_impl_CheckoutHook,
};
use alacritree_common::jobs;
use ambassador::Delegate;

#[derive(Delegate)]
#[delegate(CheckoutHook)]
enum Hook {
    First(FakeHook),
    Second(FakeHook),
}

#[test]
fn a_delegated_enum_dispatches_to_each_variant() {
    let first = FakeHook::reporting("one");
    let second = FakeHook::silent();
    let hooks = [Hook::First(first.clone()), Hook::Second(second.clone())];
    let e = Checkout { main: Path::new("/repo"), checkout: Path::new("/wt") };
    let outcomes = jobs::on_this_thread(|b| hooks[..].created(&e, b));
    assert_eq!(outcomes[0].as_ref().unwrap(), &Some("one".to_string()));
    assert_eq!(outcomes[1].as_ref().unwrap(), &None);
    assert_eq!(first.events(), [Event::Created { main: "/repo".into(), checkout: "/wt".into() }]);
    assert_eq!(second.events().len(), 1);
}
