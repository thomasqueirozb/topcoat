//! Benchmark application for the Topcoat io_uring fork.
//!
//! The handler code (pages, components, router construction) is identical
//! in every mode. The ONLY difference between the baseline and io_uring
//! builds is the entry point, selected by this crate's `io-uring`
//! feature:
//! - baseline / default: `#[tokio::main]` + `topcoat::start(...)`
//! - io_uring: plain `fn main()` + `topcoat::start_uring(...)`

use topcoat::{
    Result,
    context::Cx,
    router::{Router, RouterBuilderDiscoverExt, page},
    view::{View, component, view},
};

#[cfg(not(feature = "io-uring"))]
#[tokio::main]
async fn main() {
    topcoat::start(Router::builder().discover().build()).await.unwrap();
}

#[cfg(feature = "io-uring")]
fn main() {
    topcoat::start_uring(Router::builder().discover().build()).unwrap();
}

#[page("/")]
async fn home(cx: &Cx) -> Result<impl View> {
    Ok(view! {
        <!DOCTYPE html>
        <html>
            <head>
                <title>"Hello world"</title>
            </head>
            <body>hello(name: "World")</body>
        </html>
    })
}

#[component]
async fn hello(cx: &Cx, name: &str) -> Result<impl View> {
    Ok(view! {
        <h1>
            "Hello, "
            (name)
            "!"
        </h1>
    })
}

/// A larger page (~6 KB of markup) so responses are not only tiny ones.
#[page("/big")]
async fn big(cx: &Cx) -> Result<impl View> {
    Ok(view! {
        <!DOCTYPE html>
        <html>
            <head>
                <title>"Big page"</title>
            </head>
            <body>
                <h1>"Catalog"</h1>
                <ul>
                    item(n: 1)
                    item(n: 2)
                    item(n: 3)
                    item(n: 4)
                    item(n: 5)
                    item(n: 6)
                    item(n: 7)
                    item(n: 8)
                    item(n: 9)
                    item(n: 10)
                    item(n: 11)
                    item(n: 12)
                    item(n: 13)
                    item(n: 14)
                    item(n: 15)
                    item(n: 16)
                    item(n: 17)
                    item(n: 18)
                    item(n: 19)
                    item(n: 20)
                    item(n: 21)
                    item(n: 22)
                    item(n: 23)
                    item(n: 24)
                    item(n: 25)
                    item(n: 26)
                    item(n: 27)
                    item(n: 28)
                    item(n: 29)
                    item(n: 30)
                    item(n: 31)
                    item(n: 32)
                    item(n: 33)
                    item(n: 34)
                    item(n: 35)
                    item(n: 36)
                    item(n: 37)
                    item(n: 38)
                    item(n: 39)
                    item(n: 40)
                </ul>
            </body>
        </html>
    })
}

#[component]
async fn item(cx: &Cx, n: i32) -> Result<impl View> {
    Ok(view! {
        <li>
            <h2>"Product " (n)</h2>
            <p>"A fine product with a description long enough to take some space in the page, so the response is a few hundred bytes per item."</p>
            <p>"Price: $" (n * 3) ".99"</p>
        </li>
    })
}
